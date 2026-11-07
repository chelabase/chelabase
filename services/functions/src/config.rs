//! The service's settings, from the environment (blank values count as unset).

use std::net::SocketAddr;
use std::time::Duration;

use chelabase_storage::StorageConfig;

/// The orchestrator's bearer for `PUT /internal/bundles/...`.
pub const ORCHESTRATOR_TOKEN: &str = "CHELABASE_ORCHESTRATOR_FUNCTIONS_TOKEN";
/// The worker's bearer for `POST /internal/invoke/...` (cron and events, m7 and m8).
pub const WORKER_TOKEN: &str = "CHELABASE_WORKER_FUNCTIONS_TOKEN";

pub struct Config {
    /// `CHELABASE_FUNCTIONS_ADDR` (default `127.0.0.1:8091`): the public listener, behind the
    /// gateway.
    pub addr: SocketAddr,
    /// `CHELABASE_FUNCTIONS_INTERNAL_ADDR` (default `127.0.0.1:8092`): the internal listener's
    /// comma-separated `host:port`s, each resolved at start (every address it names is bound);
    /// compose names the service's aliases on the internal networks only, so it listens there
    /// and nowhere else.
    pub internal_addr: String,
    /// `CHELABASE_FUNCTIONS_RUNTIME_URL` (default `http://127.0.0.1:8080`).
    pub runtime_url: String,
    /// `CHELABASE_URL` in a function's env: the public API base (`CHELABASE_PUBLIC_SCHEME`,
    /// `CHELABASE_API_HOST`).
    pub public_url: String,
    /// `CHELABASE_FUNCTIONS_TIMEOUT_MS` (default 30000): an invocation's wall clock.
    pub timeout: Duration,
    /// [`ORCHESTRATOR_TOKEN`] or its `_FILE` (required).
    pub orchestrator_token: String,
    /// [`WORKER_TOKEN`] or its `_FILE` (required).
    pub worker_token: String,
    /// The bundle store (`CHELABASE_STORAGE_BACKEND`, `CHELABASE_STORAGE_S3_*`; bucket
    /// `chelabase-functions`).
    pub store: StorageConfig,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let non_blank = |name: &str| {
            get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let env_or =
            |name: &str, default: &str| non_blank(name).unwrap_or_else(|| default.to_owned());
        let token = |name: &str| -> anyhow::Result<String> {
            let value = chelabase_types::secret_env::lookup_secret(&get, name)
                .map_err(anyhow::Error::msg)?
                .ok_or_else(|| anyhow::anyhow!("{name} (or {name}_FILE) is required"))?;
            anyhow::ensure!(value.len() >= 32, "{name} must be at least 32 characters");
            Ok(value)
        };
        let orchestrator_token = token(ORCHESTRATOR_TOKEN)?;
        let worker_token = token(WORKER_TOKEN)?;
        anyhow::ensure!(
            orchestrator_token != worker_token,
            "{ORCHESTRATOR_TOKEN} and {WORKER_TOKEN} must differ: each grants its own calls"
        );
        let entry =
            chelabase_types::entry::Entry::from_lookup(&get).map_err(|e| anyhow::anyhow!("{e}"))?;
        let timeout_ms: u64 = env_or("CHELABASE_FUNCTIONS_TIMEOUT_MS", "30000")
            .parse()
            .ok()
            .filter(|ms| *ms >= 1)
            .ok_or_else(|| {
                anyhow::anyhow!("CHELABASE_FUNCTIONS_TIMEOUT_MS must be a positive integer")
            })?;
        Ok(Config {
            addr: env_or("CHELABASE_FUNCTIONS_ADDR", "127.0.0.1:8091").parse()?,
            internal_addr: env_or("CHELABASE_FUNCTIONS_INTERNAL_ADDR", "127.0.0.1:8092"),
            runtime_url: env_or("CHELABASE_FUNCTIONS_RUNTIME_URL", "http://127.0.0.1:8080"),
            public_url: entry.api_base(),
            timeout: Duration::from_millis(timeout_ms),
            orchestrator_token,
            worker_token,
            store: StorageConfig::from_lookup(&get)?,
        })
    }
}
