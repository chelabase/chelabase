//! `chelabase-init bootstrap [--demo]`: what an install needs once its services run, as a
//! one-shot inside the stack. Safe to rerun; leaves no known credential.
//!
//! # Order
//!
//! 1. Take the output dir's lock (waiting at most until the deadline).
//! 2. Wait for the metadata migrations (the orchestrator applies them) and the orchestrator.
//! 3. The identity service ([`identity`]): the `chelabase_identity` role, its password and the
//!    `identity_admin` functions ([`crate::roles`]), then the `members` pool (idempotent).
//! 4. `--demo` only: the dev member ([`dev_member`]), signed up through auth with the setup
//!    code, so it holds the seeded tenant's admin claim. Without `--demo` no admin is set: the
//!    setup code's first sign-up claims it.
//!
//! Every wait shares one deadline (`--timeout`, default 10 minutes) and retries readiness
//! only: no connection, a 502/503/504, a readiness probe's 404, an unmigrated schema. Any other
//! answer fails at once, naming the step.
//!
//! # Output dir (`--out`)
//!
//! The only place it writes: its lock and, with `--demo`, `dev-member.json` (0600, the dev
//! member's email and password). A symlink at either name fails the run before anything is
//! written. It never creates a key or a secret of its own: `chelabase-init secrets` makes those.
//!
//! # Environment
//!
//! [`ENV_KEYS`] lists everything read: the entry's hosts (`CHELABASE_AUTH_HOST`,
//! `CHELABASE_STUDIO_HOST`, `CHELABASE_API_HOST`, `CHELABASE_PUBLIC_SCHEME`), `CHELABASE_MODE`,
//! `CHELABASE_METADATA_DATABASE_URL` (its password from `CHELABASE_METADATA_DATABASE_PASSWORD_FILE`
//! when set), `CHELABASE_ORCHESTRATOR_HEALTH_URL` (default `http://orchestrator:8083/health`;
//! `none` skips the probe), the identity step's `CHELABASE_IDENTITY_URL|TOKEN_FILE|DB_PASSWORD_FILE`
//! ([`identity`]), and with `--demo` `CHELABASE_AUTH_URL` (default `http://auth:8084`) and
//! `CHELABASE_SETUP_CODE_FILE`.

pub mod dev_member;
pub mod identity;
pub mod outputs;
pub mod retry;
pub mod tenant;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use chelabase_types::mode::Mode;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;
use tracing::info;

use outputs::Outputs;
use retry::{Backoff, Check, Waiter};
use tenant::Metadata;

/// Whether the workbench's dev member is made too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demo {
    Off,
    On,
}

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
pub const DEFAULT_ORCHESTRATOR_HEALTH: &str = "http://orchestrator:8083/health";
pub const DEFAULT_AUTH_URL: &str = "http://auth:8084";

/// Every environment variable the bootstrap reads.
pub const ENV_KEYS: &[&str] = &[
    "CHELABASE_PUBLIC_SCHEME",
    "CHELABASE_STUDIO_HOST",
    "CHELABASE_API_HOST",
    "CHELABASE_AUTH_HOST",
    "CHELABASE_DOCS_HOST",
    "CHELABASE_LANDING_HOST",
    "CHELABASE_MAIL_HOST",
    "CHELABASE_MODE",
    "CHELABASE_METADATA_DATABASE_URL",
    "CHELABASE_METADATA_DATABASE_PASSWORD_FILE",
    "CHELABASE_ORCHESTRATOR_HEALTH_URL",
    "CHELABASE_IDENTITY_URL",
    "CHELABASE_IDENTITY_TOKEN_FILE",
    "CHELABASE_IDENTITY_DB_PASSWORD_FILE",
    "CHELABASE_AUTH_URL",
    "CHELABASE_SETUP_CODE_FILE",
];

/// One run's settings. No `Debug`: the metadata URL may carry its password.
pub struct Config {
    pub out_dir: PathBuf,
    pub studio_origin: String,
    pub mode: Mode,
    pub metadata_url: String,
    /// `None`: no orchestrator probe (the metadata migrations still wait).
    pub orchestrator_health: Option<String>,
    /// `--demo`: where auth answers (`CHELABASE_AUTH_URL`).
    pub auth_url: String,
    /// `--demo`: the setup code's file (`CHELABASE_SETUP_CODE_FILE`), read when it is needed.
    pub setup_code_file: Option<PathBuf>,
    /// `--demo --admin-email`: the dev member's email.
    pub admin_email: Option<String>,
    pub timeout: Duration,
    pub backoff: Backoff,
}

fn non_blank(get: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A secret file through the shared check (0600, 0400 or 0640; no symlink, nothing
/// group-writable or other-readable). Errors name the variable and path, never the value.
fn read_secret_file(name: &str, path: &str) -> Result<String> {
    use chelabase_types::file_value::{FileValue, Sensitivity, Unavailable};
    match FileValue::file(name, path, Sensitivity::SharedSecret).get() {
        Ok(v) => Ok(v.to_string()),
        Err(Unavailable::NotYet { .. }) => bail!("{name}: {path} is missing or empty"),
        Err(e) => bail!("{e}"),
    }
}

impl Config {
    /// The settings from the environment (`get`), writing into `out_dir`; `--timeout` and
    /// `--admin-email` are set by the caller.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>, out_dir: PathBuf) -> Result<Self> {
        let entry = chelabase_types::entry::Entry::from_lookup(&get)?;
        let mode = Mode::parse(get(Mode::ENV).as_deref()).map_err(anyhow::Error::msg)?;
        // The shared helper: refuses a password in the URL's user info or `password=` query
        // beside the file, and a symlinked, world-readable or empty file (0640 is fine).
        let metadata_url = chelabase_types::secret_env::url_with_password_file(
            &get,
            "CHELABASE_METADATA_DATABASE_URL",
            "CHELABASE_METADATA_DATABASE_PASSWORD_FILE",
        )
        .map_err(anyhow::Error::msg)?
        .context("CHELABASE_METADATA_DATABASE_URL is required")?;
        let orchestrator_health = match non_blank(&get, "CHELABASE_ORCHESTRATOR_HEALTH_URL") {
            Some(v) if v.eq_ignore_ascii_case("none") => None,
            Some(v) => Some(v),
            None => Some(DEFAULT_ORCHESTRATOR_HEALTH.to_string()),
        };
        Ok(Self {
            out_dir,
            studio_origin: entry.studio_origin(),
            mode,
            metadata_url,
            orchestrator_health,
            auth_url: non_blank(&get, "CHELABASE_AUTH_URL")
                .unwrap_or_else(|| DEFAULT_AUTH_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            setup_code_file: non_blank(&get, "CHELABASE_SETUP_CODE_FILE").map(PathBuf::from),
            admin_email: None,
            timeout: DEFAULT_TIMEOUT,
            backoff: Backoff::default(),
        })
    }

    /// `--demo`'s dev member settings: the setup code read now.
    pub fn dev_member(&self) -> Result<dev_member::DevMemberConfig> {
        if self.mode != Mode::SelfHosted {
            bail!("--demo is the workbench's: it needs self-hosted mode");
        }
        let file = self
            .setup_code_file
            .as_ref()
            .context("--demo needs CHELABASE_SETUP_CODE_FILE (the stack's setup code)")?;
        let code = read_secret_file("CHELABASE_SETUP_CODE_FILE", &file.display().to_string())?;
        let email = self
            .admin_email
            .as_deref()
            .map(str::trim)
            .filter(|e| !e.is_empty())
            .map(chelabase_identity::rules::normalize_email)
            .unwrap_or_else(|| dev_member::DEFAULT_EMAIL.to_string());
        Ok(dev_member::DevMemberConfig {
            auth_url: self.auth_url.clone(),
            studio_origin: self.studio_origin.clone(),
            setup_code: code.trim().to_string(),
            email,
        })
    }
}

/// A readiness probe the run waits on.
pub trait Probe: Send + Sync {
    /// What it waits for, as the deadline's error names it.
    fn name(&self) -> String;
    fn check(&self) -> impl Future<Output = Result<Check<()>>> + Send;
}

/// An HTTP readiness probe: a 2xx is ready; no answer, a 404 or a 5xx is "not yet"; any other
/// answer fails.
pub struct HttpProbe {
    http: chelabase_http::Client,
    url: String,
    what: String,
}

impl HttpProbe {
    pub fn new(what: &str, url: &str) -> Self {
        Self {
            // Readiness of this URL, not of wherever it redirects (#26): a 3xx fails the probe.
            http: chelabase_http::client_builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("the probe's HTTP client builds"),
            url: url.to_string(),
            what: what.to_string(),
        }
    }
}

impl Probe for HttpProbe {
    fn name(&self) -> String {
        format!("{} at {}", self.what, self.url)
    }

    async fn check(&self) -> Result<Check<()>> {
        match self.http.get(&self.url).send().await {
            Err(e) => Ok(Check::Wait(format!("no answer ({e})"))),
            Ok(r) if r.status().is_success() => Ok(Check::Ready(())),
            // Readiness only: a 404 (routes not up) or a gateway's 502/503/504. A 500 is an
            // answer and fails at once.
            Ok(r) if matches!(r.status().as_u16(), 404 | 502..=504) => {
                Ok(Check::Wait(format!("it answers {}", r.status())))
            }
            Ok(r) => bail!("{}: {} answers {}", self.what, self.url, r.status()),
        }
    }
}

/// No probe: always ready.
pub struct NoProbe;

impl Probe for NoProbe {
    fn name(&self) -> String {
        "nothing".into()
    }
    async fn check(&self) -> Result<Check<()>> {
        Ok(Check::Ready(()))
    }
}

/// `chelabase-init bootstrap`: the real metadata database, orchestrator, identity service and
/// (`--demo`) auth. The identity step's settings are read from the process environment
/// ([`identity`]), before anything else happens.
pub async fn run(cfg: &Config, demo: Demo) -> Result<()> {
    let identity_cfg = identity::IdentityConfig::from_lookup(|k| std::env::var(k).ok())?;
    let dev = match demo {
        Demo::On => Some(cfg.dev_member()?),
        Demo::Off => None,
    };
    let metadata = tenant::SqlMetadata::new(&cfg.metadata_url)?;
    let out = match &cfg.orchestrator_health {
        Some(url) => run_with(cfg, &metadata, &HttpProbe::new("the orchestrator", url)).await,
        None => run_with(cfg, &metadata, &NoProbe).await,
    }?;
    let w = Waiter::new(cfg.timeout, cfg.backoff);
    // After the metadata migrations (run_with waited for them): the roles' grants name the
    // `chelabase` schema they make.
    identity::run(&w, &cfg.metadata_url, &identity_cfg).await?;
    if let Some(dev) = &dev {
        dev_member::ensure(&w, &out.outputs, dev).await?;
    }
    info!("bootstrap complete");
    Ok(())
}

/// The output dir, held locked while the run lasts.
pub struct Locked {
    pub outputs: Outputs,
    _lock: std::fs::File,
}

/// The run's first part with its dependencies given: the output dir's lock, then the metadata
/// migrations and the orchestrator.
pub async fn run_with(
    cfg: &Config,
    metadata: &impl Metadata,
    orchestrator: &impl Probe,
) -> Result<Locked> {
    // Every output name is checked before anything else happens.
    let outputs = Outputs::open(&cfg.out_dir)?;
    let w = Waiter::new(cfg.timeout, cfg.backoff);
    let lock_path = outputs.path().join(outputs::LOCK);
    let lock = w
        .until(
            &format!("the output dir's lock {}", lock_path.display()),
            || async {
                Ok(match outputs.try_lock()? {
                    Some(file) => Check::Ready(file),
                    None => Check::Wait("another bootstrap holds it".into()),
                })
            },
        )
        .await?;
    w.until(
        &format!(
            "the metadata database's migrations, applied by {}",
            orchestrator.name()
        ),
        || async {
            Ok(match metadata.migrated().await? {
                Check::Wait(reason) => Check::Wait(format!(
                    "{reason}. If the orchestrator is up, it may run an older build that \
                     doesn't know these migrations: recreate it on the current build"
                )),
                ready => ready,
            })
        },
    )
    .await?;
    w.until(&orchestrator.name(), || orchestrator.check())
        .await?;
    Ok(Locked { outputs, _lock: lock })
}
