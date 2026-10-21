//! Realtime's client pair for the replication session (W31, SP9): `chelabase_realtime` logs in
//! only over TLS with this certificate (CN `chelabase-realtime`). It is read from
//! `CHELABASE_REALTIME_TLS_DIR`, where the worker's daily sweep writes and renews it; a task
//! reloads it within a minute of a change, so a renewal needs no restart and a session started
//! after it presents the new certificate.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chelabase_neon::compute_tls::ClientTls;
use tracing::{info, warn};

/// How often the pair is checked for a renewal.
pub const RELOAD_EVERY: Duration = Duration::from_secs(60);

/// The loaded pair, or none yet (the worker writes the first one; until then replication
/// sessions are unavailable and retried).
pub struct ClientPair {
    dir: PathBuf,
    loaded: RwLock<Option<Arc<ClientTls>>>,
}

impl ClientPair {
    /// Loads `<dir>/current/*`; a missing or broken pair is logged, and retried by
    /// [`ClientPair::refresh`].
    pub fn new(dir: PathBuf) -> Self {
        let loaded = match ClientTls::load(&dir) {
            Ok(tls) => Some(Arc::new(tls)),
            Err(err) => {
                warn!("no realtime client certificate yet: {err:#}");
                None
            }
        };
        ClientPair {
            dir,
            loaded: RwLock::new(loaded),
        }
    }

    /// The loaded pair.
    pub fn get(&self) -> Option<Arc<ClientTls>> {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Loads the pair when none is loaded, otherwise swaps in a renewed one. Errors are logged;
    /// the pair that was loaded stays.
    pub fn refresh(&self) {
        match self.get() {
            Some(tls) => match tls.reload_if_changed() {
                Ok(true) => info!("realtime client certificate reloaded"),
                Ok(false) => {}
                Err(err) => warn!("reloading the realtime client certificate: {err:#}"),
            },
            None => match ClientTls::load(&self.dir) {
                Ok(tls) => {
                    info!("realtime client certificate loaded");
                    *self.loaded.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(tls));
                }
                Err(err) => warn!("no realtime client certificate yet: {err:#}"),
            },
        }
    }

    /// Runs [`ClientPair::refresh`] every [`RELOAD_EVERY`], for the life of the process.
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
