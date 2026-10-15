//! The daily certificate sweep (RT3-7): each running compute's server certificate, and the
//! orchestrator's client pair, are reissued before they expire.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::Context;
use chelabase_neon::compute_tls::{
    CLIENT_CERT_FILE, CLIENT_RENEW_BEFORE, ComputeCa, ORCHESTRATOR_CN, PGDATA_SERVER_CERT_PATH,
    SERVER_RENEW_BEFORE, needs_renewal, not_after, write_client_pair,
};
use chelabase_neon::{ComputeBackend, ComputeStatus};
use tracing::{info, warn};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// How often the sweep runs.
pub const CERT_SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// What the sweep needs beyond the compute backend: the CA that signs, and the directory the
/// orchestrator's client pair lives in.
#[derive(Clone)]
pub struct CertSweep {
    pub ca: Arc<ComputeCa>,
    /// `CHELABASE_COMPUTE_TLS_DIR`.
    pub tls_dir: PathBuf,
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// One pass: every running compute whose served `server.crt` (the copy compute_ctl made in
    /// `pgdata`, not the upload: a reload that failed leaves a fresh upload next to an old
    /// copy) has under 30 days left gets a new one
    /// (uploaded; compute_ctl reloads Postgres with it and the status never leaves `running`),
    /// and the orchestrator's client pair is reissued with under 60 days left. Returns
    /// `(renewed, failed)`; a failure is logged and the next pass retries.
    pub async fn renew_certs_once(&self, now: SystemTime) -> (usize, usize) {
        let Some(sweep) = &self.cert_sweep else {
            return (0, 0);
        };
        let (mut renewed, mut failed) = (0, 0);
        let endpoints = match self.computes.list().await {
            Ok(endpoints) => endpoints,
            Err(e) => {
                warn!("certificate sweep: listing the computes: {e:#}");
                return (0, 1);
            }
        };
        for ep in endpoints {
            match self.renew_server_cert(sweep, &ep, now).await {
                Ok(true) => renewed += 1,
                Ok(false) => {}
                Err(e) => {
                    failed += 1;
                    warn!(endpoint_id = %ep, "certificate sweep: {e:#}");
                }
            }
        }
        match renew_orchestrator_pair(sweep, now) {
            Ok(true) => renewed += 1,
            Ok(false) => {}
            Err(e) => {
                failed += 1;
                warn!("certificate sweep: the orchestrator's client certificate: {e:#}");
            }
        }
        (renewed, failed)
    }

    /// `true` when `ep` got a new certificate; `false` when it isn't running or still has time.
    async fn renew_server_cert(
        &self,
        sweep: &CertSweep,
        ep: &str,
        now: SystemTime,
    ) -> anyhow::Result<bool> {
        if !matches!(
            self.computes.status(ep).await?,
            Some(ComputeStatus::Running)
        ) {
            return Ok(false);
        }
        let pem = self
            .computes
            .read_file(ep, PGDATA_SERVER_CERT_PATH)
            .await
            .context("reading the served server.crt")?;
        let pem = String::from_utf8(pem).context("the served server.crt isn't UTF-8")?;
        let expires = not_after(&pem).context("reading server.crt's expiry")?;
        if !needs_renewal(expires, now, SERVER_RENEW_BEFORE) {
            return Ok(false);
        }
        let cert = sweep
            .ca
            .issue_server(ep, SystemTime::now())
            .context("issuing a certificate")?;
        self.computes
            .upload_tls(ep, &cert)
            .await
            .context("uploading the certificate")?;
        info!(endpoint_id = %ep, "renewed the compute's server certificate");
        Ok(true)
    }
}

/// `true` when a new pair was written. A missing pair is written too.
fn renew_orchestrator_pair(sweep: &CertSweep, now: SystemTime) -> anyhow::Result<bool> {
    let current = sweep.tls_dir.join("current").join(CLIENT_CERT_FILE);
    let due = match std::fs::read_to_string(&current) {
        // A client.crt that can't be read as a certificate is reissued, not an error.
        Ok(pem) => match not_after(&pem) {
            Ok(expires) => needs_renewal(expires, now, CLIENT_RENEW_BEFORE),
            Err(e) => {
                warn!("certificate sweep: {} is corrupt ({e:#}); reissuing", current.display());
                true
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => return Err(e).with_context(|| format!("reading {}", current.display())),
    };
    if !due {
        return Ok(false);
    }
    let pair = sweep.ca.issue_client(ORCHESTRATOR_CN, now)?;
    write_client_pair(&sweep.tls_dir, &pair, sweep.ca.cert_pem(), now)?;
    info!("reissued the orchestrator's client certificate");
    Ok(true)
}
