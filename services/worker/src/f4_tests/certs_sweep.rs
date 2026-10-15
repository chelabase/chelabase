//! T9 I3, m4, m5: the daily certificate sweep (RT3-7), over the fakes.
//!
//! - The sweep judges a compute's renewal from the copy Postgres serves (`pgdata`), not from the
//!   uploaded `server.crt`: a reload that failed leaves a fresh upload next to an old copy, and
//!   the sweep must upload again.
//! - A corrupt `client.crt` is reissued, not an error.
//! - `ComputeBackend::read_file` and `upload_tls` are the backend's trait methods, so the sweep
//!   runs against any backend (the fake keeps `files`, `tls_uploads`, `fail_upload`).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chelabase_neon::ComputeBackend;
use chelabase_neon::compute_tls::{
    ClientTls, ComputeCa, ORCHESTRATOR_CN, PGDATA_SERVER_CERT_PATH, SERVER_CERT_PATH,
    SERVER_VALIDITY, not_after, write_client_pair,
};

use crate::certs::CertSweep;
use crate::fakes::{FakeWorker, worker};

const DAY: Duration = Duration::from_secs(86_400);

struct Rig {
    w: FakeWorker,
    ca: Arc<ComputeCa>,
    dir: tempfile::TempDir,
    now: SystemTime,
}

/// A worker with a sweep, a fresh orchestrator client pair on disk, and no computes.
fn rig() -> Rig {
    let now = SystemTime::now();
    let ca = Arc::new(ComputeCa::generate(now).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
    let (mut w, _journal) = worker();
    w.cert_sweep = Some(CertSweep {
        ca: ca.clone(),
        tls_dir: dir.path().to_path_buf(),
    });
    Rig { w, ca, dir, now }
}

impl Rig {
    /// A running compute whose uploaded certificate was issued `uploaded_age` ago and whose
    /// `pgdata` copy `served_age` ago.
    async fn compute(&self, ep: &str, uploaded_age: Duration, served_age: Duration) {
        self.w.computes.create(ep, "t").await.unwrap();
        let uploaded = self.ca.issue_server(ep, self.now - uploaded_age).unwrap();
        let served = self.ca.issue_server(ep, self.now - served_age).unwrap();
        self.w
            .computes
            .set_file(ep, SERVER_CERT_PATH, uploaded.cert_pem.as_bytes());
        self.w
            .computes
            .set_file(ep, PGDATA_SERVER_CERT_PATH, served.cert_pem.as_bytes());
    }

    fn uploaded(&self) -> Vec<String> {
        self.w
            .computes
            .tls_uploads
            .lock()
            .unwrap()
            .iter()
            .map(|(ep, _)| ep.clone())
            .collect()
    }
}

fn client_cert(dir: &Path) -> std::path::PathBuf {
    dir.join("current").join("client.crt")
}

#[tokio::test]
async fn a_certificate_with_under_thirty_days_left_in_pgdata_is_renewed() {
    let r = rig();
    r.compute("ep-a", DAY, 70 * DAY).await; // the copy has 20 days left
    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));
    assert_eq!(r.uploaded(), ["ep-a"]);
    let uploads = r.w.computes.tls_uploads.lock().unwrap();
    assert!(not_after(&uploads[0].1.cert_pem).unwrap() > r.now + 80 * DAY);
}

#[tokio::test]
async fn the_sweep_judges_what_postgres_serves_not_what_was_uploaded() {
    // A fresh upload next to a stale copy: the reload failed, so upload again.
    let r = rig();
    r.compute("ep-stale-copy", DAY, 70 * DAY).await;
    // An old upload next to a fresh copy: Postgres serves a good certificate, nothing to do.
    r.compute("ep-fresh-copy", 70 * DAY, DAY).await;
    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));
    assert_eq!(r.uploaded(), ["ep-stale-copy"]);
}

#[tokio::test]
async fn a_fresh_compute_is_left_alone() {
    let r = rig();
    r.compute("ep-a", DAY, DAY).await;
    assert_eq!(r.w.renew_certs_once(r.now).await, (0, 0));
    assert!(r.uploaded().is_empty());
}

#[tokio::test]
async fn a_compute_that_isnt_running_is_skipped() {
    let r = rig();
    r.compute("ep-a", DAY, 70 * DAY).await;
    r.w.computes.set_running("ep-a", false);
    assert_eq!(r.w.renew_certs_once(r.now).await, (0, 0));
    assert!(r.uploaded().is_empty());
}

#[tokio::test]
async fn a_running_compute_without_a_served_certificate_is_a_failure_not_a_renewal() {
    let r = rig();
    r.w.computes.create("ep-a", "t").await.unwrap();
    // Only the upload exists: what Postgres serves can't be read.
    let uploaded = r.ca.issue_server("ep-a", r.now - 70 * DAY).unwrap();
    r.w.computes
        .set_file("ep-a", SERVER_CERT_PATH, uploaded.cert_pem.as_bytes());
    assert_eq!(r.w.renew_certs_once(r.now).await, (0, 1));
    assert!(r.uploaded().is_empty());
}

#[tokio::test]
async fn a_corrupt_served_certificate_is_a_failure() {
    let r = rig();
    r.w.computes.create("ep-a", "t").await.unwrap();
    // A good upload next to a copy that isn't a certificate: only the copy is judged.
    let uploaded = r.ca.issue_server("ep-a", r.now).unwrap();
    r.w.computes
        .set_file("ep-a", SERVER_CERT_PATH, uploaded.cert_pem.as_bytes());
    r.w.computes
        .set_file("ep-a", PGDATA_SERVER_CERT_PATH, b"not a certificate");
    assert_eq!(r.w.renew_certs_once(r.now).await, (0, 1));
}

#[tokio::test]
async fn a_failed_upload_is_counted_and_retried_by_the_next_pass() {
    let r = rig();
    r.compute("ep-a", DAY, 70 * DAY).await;
    r.compute("ep-b", DAY, DAY).await;
    *r.w.computes.fail_upload.lock().unwrap() = Some("the container is gone".into());
    assert_eq!(r.w.renew_certs_once(r.now).await, (0, 1));
    assert!(r.uploaded().is_empty());
    *r.w.computes.fail_upload.lock().unwrap() = None;
    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));
    assert_eq!(r.uploaded(), ["ep-a"]);
}

#[tokio::test]
async fn the_renewed_certificate_is_a_new_pair_with_a_full_lifetime() {
    let r = rig();
    r.compute("ep-a", DAY, 70 * DAY).await;
    r.w.renew_certs_once(r.now).await;
    let uploads = r.w.computes.tls_uploads.lock().unwrap();
    let (ep, cert) = &uploads[0];
    assert_eq!(ep, "ep-a");
    // A new pair, not the old copy, with its own key.
    let old = r.w.computes.files.lock().unwrap();
    assert_ne!(
        old[&("ep-a".to_string(), PGDATA_SERVER_CERT_PATH.to_string())],
        cert.cert_pem.as_bytes()
    );
    assert!(cert.key_pem.starts_with("-----BEGIN PRIVATE KEY-----"));
    assert!(!chelabase_neon::compute_tls::needs_renewal(
        cert.not_after,
        r.now,
        30 * DAY
    ));
}

#[tokio::test]
async fn an_orchestrator_pair_with_under_sixty_days_left_is_reissued() {
    let now = SystemTime::now();
    let ca = Arc::new(ComputeCa::generate(now - 400 * DAY).unwrap());
    let dir = tempfile::tempdir().unwrap();
    // Issued 320 days ago: 45 days left of 365.
    let then = now - 320 * DAY;
    let old = ca.issue_client(ORCHESTRATOR_CN, then).unwrap();
    write_client_pair(dir.path(), &old, ca.cert_pem(), then).unwrap();
    let before = ClientTls::load(dir.path()).unwrap().fingerprint();
    let (mut w, _journal) = worker();
    w.cert_sweep = Some(CertSweep {
        ca: ca.clone(),
        tls_dir: dir.path().to_path_buf(),
    });
    assert_eq!(w.renew_certs_once(now).await, (1, 0));
    let tls = ClientTls::load(dir.path()).unwrap();
    assert_ne!(tls.fingerprint(), before);
}

#[tokio::test]
async fn a_corrupt_client_certificate_is_reissued() {
    let r = rig();
    let path = client_cert(r.dir.path());
    let before = std::fs::read(&path).unwrap();
    // Through the `current` link, so the version directory's file is the corrupt one.
    std::fs::write(
        &path,
        b"-----BEGIN CERTIFICATE-----\nnot base64 !!\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    assert_eq!(
        r.w.renew_certs_once(r.now).await,
        (1, 0),
        "reissued, not an error"
    );
    let after = std::fs::read(client_cert(r.dir.path())).unwrap();
    assert_ne!(after, before);
    not_after(std::str::from_utf8(&after).unwrap()).expect("the new certificate parses");
    ClientTls::load(r.dir.path()).expect("the pair on disk loads");
}

#[tokio::test]
async fn an_empty_client_certificate_file_is_reissued_too() {
    let r = rig();
    std::fs::write(client_cert(r.dir.path()), b"").unwrap();
    assert_eq!(r.w.renew_certs_once(r.now).await, (1, 0));
    ClientTls::load(r.dir.path()).unwrap();
}

#[tokio::test]
async fn without_a_sweep_nothing_happens() {
    let (w, _journal) = worker();
    assert_eq!(w.renew_certs_once(SystemTime::now()).await, (0, 0));
    // The trait the sweep reads files through exists on the fake too.
    w.computes.create("ep-a", "t").await.unwrap();
    assert!(
        w.computes
            .read_file("ep-a", SERVER_CERT_PATH)
            .await
            .is_err(),
        "a file never set"
    );
}

#[tokio::test]
async fn a_sweep_judged_in_the_future_still_issues_at_the_wall_clock() {
    // The sweep judges renewal at the time it is given (a test's clock 70 days ahead), but the
    // certificate it issues must be valid now: a `not_before` in the future is refused by the
    // compute (compute_ctl checks validity) and the renewal never takes effect.
    let r = rig();
    r.compute("ep-a", DAY, DAY).await;
    let judge_at = r.now + 70 * DAY;
    assert_eq!(r.w.renew_certs_once(judge_at).await, (1, 0));
    let uploads = r.w.computes.tls_uploads.lock().unwrap();
    let issued_from = not_after(&uploads[0].1.cert_pem).unwrap() - SERVER_VALIDITY;
    assert!(
        issued_from <= SystemTime::now(),
        "the certificate was issued for a time ahead of the wall clock: {issued_from:?}"
    );
}
