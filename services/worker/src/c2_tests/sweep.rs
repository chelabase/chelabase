//! SP10: the daily sweep renews both client pairs, the orchestrator's and realtime's, through
//! `compute_tls::renew_client_pair`, and writes a missing one. (The server-certificate cases are
//! `f4_tests/certs_sweep.rs`; its rig writes a realtime pair so its counts hold.)

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chelabase_neon::compute_tls::{
    ClientTls, ComputeCa, ORCHESTRATOR_CN, REALTIME_CN, renew_client_pair, write_client_pair,
};

use crate::certs::CertSweep;
use crate::fakes::{FakeWorker, worker};

const DAY: Duration = Duration::from_secs(86_400);

struct Rig {
    w: FakeWorker,
    ca: Arc<ComputeCa>,
    orchestrator: tempfile::TempDir,
    realtime: tempfile::TempDir,
    now: SystemTime,
}

/// A CA, and two directories: the orchestrator's and realtime's. `*_age` is how long ago that
/// pair was issued (`None`: no pair on disk). A client pair lives 365 days and is due under 60
/// days before its end, so an age above 305 days is due.
fn rig(orchestrator_age: Option<Duration>, realtime_age: Option<Duration>) -> Rig {
    let now = SystemTime::now();
    let ca = Arc::new(ComputeCa::generate(now - 400 * DAY).unwrap());
    let orchestrator = tempfile::tempdir().unwrap();
    let realtime = tempfile::tempdir().unwrap();
    for (dir, cn, age) in [
        (&orchestrator, ORCHESTRATOR_CN, orchestrator_age),
        (&realtime, REALTIME_CN, realtime_age),
    ] {
        if let Some(age) = age {
            write_pair(&ca, dir.path(), cn, now - age);
        }
    }
    let (mut w, _journal) = worker();
    w.cert_sweep = Some(CertSweep {
        ca: ca.clone(),
        tls_dir: orchestrator.path().to_path_buf(),
        realtime_tls_dir: realtime.path().to_path_buf(),
    });
    Rig {
        w,
        ca,
        orchestrator,
        realtime,
        now,
    }
}

fn write_pair(ca: &ComputeCa, dir: &Path, cn: &str, issued: SystemTime) {
    let pair = ca.issue_client(cn, issued).unwrap();
    write_client_pair(dir, &pair, ca.cert_pem(), issued).unwrap();
}

fn fingerprint(dir: &Path) -> [u8; 32] {
    ClientTls::load(dir).unwrap().fingerprint()
}

#[tokio::test]
async fn both_pairs_due_are_both_renewed() {
    let r = rig(Some(320 * DAY), Some(320 * DAY));
    let (o_before, r_before) = (
        fingerprint(r.orchestrator.path()),
        fingerprint(r.realtime.path()),
    );

    assert_eq!(r.w.renew_certs_once(r.now).await, (2, 0));

    assert_ne!(fingerprint(r.orchestrator.path()), o_before);
    assert_ne!(fingerprint(r.realtime.path()), r_before);
    // Each directory holds a pair for its own CN, current for a year.
    assert!(!renew_client_pair(r.orchestrator.path(), &r.ca, ORCHESTRATOR_CN, r.now).unwrap());
    assert!(!renew_client_pair(r.realtime.path(), &r.ca, REALTIME_CN, r.now).unwrap());
}

#[tokio::test]
async fn only_the_due_pair_is_renewed() {
    let r = rig(Some(DAY), Some(320 * DAY));
    let o_before = fingerprint(r.orchestrator.path());

    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));

    assert_eq!(
        fingerprint(r.orchestrator.path()),
        o_before,
        "a good pair is left alone"
    );
    assert!(!renew_client_pair(r.realtime.path(), &r.ca, REALTIME_CN, r.now).unwrap());

    let r = rig(Some(320 * DAY), Some(DAY));
    let r_before = fingerprint(r.realtime.path());
    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));
    assert_eq!(fingerprint(r.realtime.path()), r_before);
}

#[tokio::test]
async fn nothing_is_renewed_when_both_pairs_are_good() {
    let r = rig(Some(DAY), Some(DAY));
    assert_eq!(r.w.renew_certs_once(r.now).await, (0, 0));
}

#[tokio::test]
async fn a_missing_realtime_pair_is_written() {
    let r = rig(Some(DAY), None);
    assert!(!r.realtime.path().join("current").exists());

    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));

    ClientTls::load(r.realtime.path()).expect("the pair on disk loads");
    assert!(!renew_client_pair(r.realtime.path(), &r.ca, REALTIME_CN, r.now).unwrap());
    // The second pass finds it current.
    assert_eq!(r.w.renew_certs_once(r.now).await, (0, 0));
}

#[tokio::test]
async fn a_missing_orchestrator_pair_is_still_written() {
    let r = rig(None, Some(DAY));
    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));
    ClientTls::load(r.orchestrator.path()).unwrap();
    assert!(!renew_client_pair(r.orchestrator.path(), &r.ca, ORCHESTRATOR_CN, r.now).unwrap());
}

#[tokio::test]
async fn a_pair_with_the_wrong_cn_is_reissued_for_the_right_one() {
    // renew_client_pair judges the CN, the CA and the key too, not only the expiry: an
    // orchestrator pair copied into realtime's directory must not stand in for it.
    let r = rig(Some(DAY), None);
    write_pair(&r.ca, r.realtime.path(), ORCHESTRATOR_CN, r.now - DAY);
    let before = fingerprint(r.realtime.path());

    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));

    assert_ne!(fingerprint(r.realtime.path()), before);
    assert!(!renew_client_pair(r.realtime.path(), &r.ca, REALTIME_CN, r.now).unwrap());
}

#[tokio::test]
async fn a_pair_signed_by_another_ca_is_reissued() {
    let r = rig(Some(DAY), None);
    let foreign = ComputeCa::generate(r.now - DAY).unwrap();
    write_pair(&foreign, r.realtime.path(), REALTIME_CN, r.now - DAY);
    let before = fingerprint(r.realtime.path());

    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));

    assert_ne!(fingerprint(r.realtime.path()), before);
    assert!(!renew_client_pair(r.realtime.path(), &r.ca, REALTIME_CN, r.now).unwrap());
}

#[tokio::test]
async fn a_corrupt_realtime_certificate_is_reissued_not_an_error() {
    let r = rig(Some(DAY), Some(DAY));
    std::fs::write(r.realtime.path().join("current").join("client.crt"), b"").unwrap();

    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));

    ClientTls::load(r.realtime.path()).unwrap();
}

#[tokio::test]
async fn a_realtime_failure_is_counted_and_does_not_stop_the_orchestrators_renewal() {
    let mut r = rig(Some(320 * DAY), None);
    // A directory that can't be created: its parent is a regular file.
    let file = r.orchestrator.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let sweep = r.w.cert_sweep.as_mut().unwrap();
    sweep.realtime_tls_dir = file.join("realtime-tls");
    let before = fingerprint(r.orchestrator.path());

    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 1));

    assert_ne!(fingerprint(r.orchestrator.path()), before);
}

#[tokio::test]
async fn an_orchestrator_failure_does_not_stop_the_realtime_renewal() {
    let mut r = rig(None, Some(320 * DAY));
    let file = r.realtime.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let sweep = r.w.cert_sweep.as_mut().unwrap();
    sweep.tls_dir = file.join("orchestrator-tls");
    let before = fingerprint(r.realtime.path());

    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 1));

    assert_ne!(fingerprint(r.realtime.path()), before);
}
