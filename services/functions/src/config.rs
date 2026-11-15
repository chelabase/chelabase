//! The service's settings, from the environment (blank values count as unset).

use std::time::Duration;

use chelabase_storage::StorageConfig;

/// The orchestrator's bearer for `PUT /internal/bundles/...`.
pub const ORCHESTRATOR_TOKEN: &str = "CHELABASE_ORCHESTRATOR_FUNCTIONS_TOKEN";
/// The worker's bearer for `POST /internal/invoke/...` (cron and events, m7 and m8).
pub const WORKER_TOKEN: &str = "CHELABASE_WORKER_FUNCTIONS_TOKEN";

/// m11: this service's bearer for the service runner.
pub const RUNNER_TOKEN: &str = "CHELABASE_FUNCTIONS_SERVICE_RUNNER_TOKEN";

pub struct Config {
    /// `CHELABASE_FUNCTIONS_ADDR` (default `127.0.0.1:8091`): the public listener, behind the
    /// gateway; comma-separated `host:port`s resolved at start like the internal one (compose
    /// names the service's alias on the default network).
    pub addr: String,
    /// `CHELABASE_FUNCTIONS_INTERNAL_ADDR` (default `127.0.0.1:8092`): the internal listener's
    /// comma-separated `host:port`s, each resolved at start (every address it names is bound);
    /// compose names the service's aliases on the internal networks only, so it listens there
    /// and nowhere else.
    pub internal_addr: String,
    /// `CHELABASE_FUNCTIONS_CONTROL_ADDR` (default `127.0.0.1:8093`): the internal listener's
    /// control side (bundle uploads and invocations), listed and resolved like
    /// `CHELABASE_FUNCTIONS_INTERNAL_ADDR`, which serves bundle GETs only.
    pub control_addr: String,
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
    /// m13b: `CHELABASE_INTERNAL_URL` (default `http://127.0.0.1:8083`): the orchestrator's
    /// internal listener, asked for the dispatcher login's connection info with
    /// [`ORCHESTRATOR_TOKEN`] (both ways, as rest's bearer).
    pub internal_url: String,
    /// m13b: `CHELABASE_COMPUTE_CA_FILE`: every compute is verified against it. Unset: database
    /// events aren't dispatched.
    pub compute_ca: Option<std::path::PathBuf>,
    /// m11: `CHELABASE_SERVICE_RUNNER_URL` and `CHELABASE_FUNCTIONS_SERVICE_RUNNER_TOKEN` (or its
    /// `_FILE`): the service runner and this service's bearer for it. Either unset: container
    /// services answer `runner_unavailable`.
    pub runner_url: Option<String>,
    pub runner_token: Option<String>,
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
            addr: env_or("CHELABASE_FUNCTIONS_ADDR", "127.0.0.1:8091"),
            internal_addr: env_or("CHELABASE_FUNCTIONS_INTERNAL_ADDR", "127.0.0.1:8092"),
            control_addr: env_or("CHELABASE_FUNCTIONS_CONTROL_ADDR", "127.0.0.1:8093"),
            runtime_url: env_or("CHELABASE_FUNCTIONS_RUNTIME_URL", "http://127.0.0.1:8080"),
            public_url: entry.api_base(),
            timeout: Duration::from_millis(timeout_ms),
            orchestrator_token,
            worker_token,
            internal_url: env_or("CHELABASE_INTERNAL_URL", "http://127.0.0.1:8083"),
            compute_ca: non_blank("CHELABASE_COMPUTE_CA_FILE").map(Into::into),
            runner_url: non_blank("CHELABASE_SERVICE_RUNNER_URL"),
            runner_token: chelabase_types::secret_env::lookup_secret(&get, RUNNER_TOKEN)
                .map_err(anyhow::Error::msg)?
                .filter(|t| !t.trim().is_empty()),
            store: StorageConfig::from_lookup(&get)?,
        })
    }
}
