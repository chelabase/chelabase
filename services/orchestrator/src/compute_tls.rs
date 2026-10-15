//! The orchestrator's client pair for TLS to computes (RT3-7): loaded from
//! `CHELABASE_COMPUTE_TLS_DIR`, where the worker's daily sweep renews it; a task reloads it
//! within a minute of a change, so a renewal needs no restart.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chelabase_neon::compute_tls::ClientTls;
use tracing::{info, warn};

/// How often the pair is checked for a renewal.
pub const RELOAD_EVERY: Duration = Duration::from_secs(60);

/// The loaded pair, or none yet (the worker writes the first one; until then connections to
/// computes fail with `compute_tls_failed`).
pub struct ComputeTls {
    dir: PathBuf,
    loaded: RwLock<Option<Arc<ClientTls>>>,
}

impl ComputeTls {
    /// Loads `<dir>/current/*`; a missing or broken pair is logged, and retried by [`refresh`].
    ///
    /// [`refresh`]: ComputeTls::refresh
    pub fn new(dir: PathBuf) -> Self {
        let loaded = match ClientTls::load(&dir) {
            Ok(tls) => Some(Arc::new(tls)),
            Err(err) => {
                warn!("no compute client certificate yet: {err:#}");
                None
            }
        };
        ComputeTls { dir, loaded: RwLock::new(loaded) }
    }

    /// The loaded pair.
    pub fn get(&self) -> Option<Arc<ClientTls>> {
        self.loaded.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Loads the pair when none is loaded, otherwise swaps in a renewed one. Errors are logged;
    /// the pair that was loaded stays.
    pub fn refresh(&self) {
        match self.get() {
            Some(tls) => match tls.reload_if_changed() {
                Ok(true) => info!("compute client certificate reloaded"),
                Ok(false) => {}
                Err(err) => warn!("reloading the compute client certificate: {err:#}"),
            },
            None => match ClientTls::load(&self.dir) {
                Ok(tls) => {
                    info!("compute client certificate loaded");
                    *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(tls));
                }
                Err(err) => warn!("no compute client certificate yet: {err:#}"),
            },
        }
    }

    /// Runs [`refresh`](ComputeTls::refresh) every [`RELOAD_EVERY`], for the life of the process.
    pub fn spawn_reload(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(RELOAD_EVERY);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                let this = Arc::clone(&this);
                // File reads and a key parse: off the async threads.
                let _ = tokio::task::spawn_blocking(move || this.refresh()).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::OwnerPoolKey;
    use crate::sql_editor::run::RunTls;
    use chelabase_neon::compute_tls::{ComputeCa, ORCHESTRATOR_CN, write_client_pair};
    use std::time::SystemTime;

    #[test]
    fn a_pair_that_appears_later_is_loaded_and_a_renewal_changes_the_pool_key() {
        let dir = tempfile::tempdir().unwrap();
        let tls = ComputeTls::new(dir.path().to_path_buf());
        assert!(tls.get().is_none(), "no pair yet");
        let now = SystemTime::now();
        let ca = ComputeCa::generate(now).unwrap();
        let first = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
        write_client_pair(dir.path(), &first, ca.cert_pem(), now).unwrap();
        tls.refresh();
        let loaded = tls.get().expect("loaded by refresh");
        let branch = chelabase_types::BranchId::new();
        let key = |tls: &Arc<ClientTls>| {
            let run = RunTls::Compute { server_name: "ep.x".into(), client: tls.clone() };
            OwnerPoolKey::new(branch, "d", "s", "127.0.0.1:5432".parse().unwrap(), &run)
        };
        let key_before = key(&loaded);
        let before = loaded.fingerprint();
        let later = now + Duration::from_secs(1);
        let second = ca.issue_client(ORCHESTRATOR_CN, later).unwrap();
        write_client_pair(dir.path(), &second, ca.cert_pem(), later).unwrap();
        tls.refresh();
        assert_ne!(tls.get().unwrap().fingerprint(), before, "a renewed pair is swapped in");
        assert_ne!(key_before, key(&tls.get().unwrap()), "a renewed pair is a new pool");
    }

    // T10 m3: a pool's key and its connector come from one snapshot of the pair, so a reload
    // between the two reads can't pair one certificate's fingerprint with another's
    // connector.
    #[test]
    fn a_snapshot_keeps_the_pair_it_was_taken_from() {
        use chelabase_neon::compute_tls::ClientTlsSnapshot;
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let ca = ComputeCa::generate(now).unwrap();
        let first = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
        write_client_pair(dir.path(), &first, ca.cert_pem(), now).unwrap();
        let client = ClientTls::load(dir.path()).unwrap();
        let snapshot: ClientTlsSnapshot = client.snapshot();
        let before = client.fingerprint();
        assert_eq!(snapshot.fingerprint(), before);
        let later = now + Duration::from_secs(1);
        let second = ca.issue_client(ORCHESTRATOR_CN, later).unwrap();
        write_client_pair(dir.path(), &second, ca.cert_pem(), later).unwrap();
        assert!(client.reload_if_changed().unwrap());
        assert_ne!(client.fingerprint(), before);
        // The snapshot taken before the reload is still the first pair, whole.
        assert_eq!(snapshot.clone().fingerprint(), before);
        // (a `MakeRustlsConnect` over the snapshot's config)
        let _connector = snapshot.connector();
        assert_eq!(client.snapshot().fingerprint(), client.fingerprint());
    }
}
