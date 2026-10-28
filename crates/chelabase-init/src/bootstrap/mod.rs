//! `chelabase-init bootstrap [--demo]`: the product's Zitadel objects and the seeded tenant's
//! bind, as a one-shot inside the stack. Safe to rerun; leaves no known credential.
//!
//! # Order
//!
//! 1. Wait for the admin PAT file and for Zitadel (`/debug/ready`, then the first API call).
//! 2. Without `--demo`: deactivate Zitadel's FirstInstance human (an IAM owner whose random
//!    password is only in `chelabase-init secrets`' `zitadel/steps.yaml`), before any wait on
//!    the orchestrator. With `--demo` it stays: it is the workbench's sign-in.
//! 3. The default org, the `chelabase` project, the API app, the Studio app and its login,
//!    Zitadel's own registration off.
//! 4. `--demo` only: the demo machine user and its key (`demo-sa.json`), Mailpit as SMTP.
//!    SMTP from `CHELABASE_SMTP_*` when set (either mode); otherwise none.
//! 5. Wait for the metadata migrations (the orchestrator applies them) and the orchestrator.
//! 6. Bind the seeded tenant to the org (only when unbound or already this org: a tenant bound
//!    to another org fails, naming both) and name it after the org when it has no name.
//! 7. `--demo` only: the tenant admins (the requested email, else Zitadel's first human; and
//!    the demo machine user in self-hosted mode). Without `--demo` no admin is set: the setup
//!    code's claim decides it.
//! 8. The output ids, then the Actions target last (its host, the orchestrator, must resolve).
//!
//! Every wait shares one deadline (`--timeout`, default 10 minutes) and retries readiness
//! only: no connection, a 502/503/504, a readiness probe's 404, an unmigrated schema, an
//! Actions target host that doesn't resolve yet. Any other answer (a 4xx on a valid request,
//! a 404 on an object, a conflicting object) fails at once, naming the step.
//!
//! # Output dir (`--out`)
//!
//! The only place it writes. The apps read these files (m5: through `*_FILE`); none becomes an
//! env file or a container env var. A symlink at any of these names fails the run before
//! anything is written.
//!
//! | File | What | Mode |
//! |---|---|---|
//! | `org-id` | the Zitadel org the tenant is bound to | 0644 |
//! | `project-id` | the `chelabase` project | 0644 |
//! | `api-client-id` | the API app's client id (a token audience) | 0644 |
//! | `studio-client-id` | Studio's OIDC client id | 0644 |
//! | `actions-signing-key` | the Actions webhook's signing key | 0600 |
//! | `demo-user-id` | `--demo`: the demo machine user | 0644 |
//! | `demo-sa.json` | `--demo`: the demo machine user's key file | 0600 |
//!
//! It never creates a key or a secret of its own: `chelabase-init secrets` makes those.
//!
//! # Environment
//!
//! [`ENV_KEYS`] lists everything read. The entry's hosts (`CHELABASE_AUTH_HOST`,
//! `CHELABASE_STUDIO_HOST`, `CHELABASE_API_HOST`, `CHELABASE_PUBLIC_SCHEME`), `CHELABASE_MODE`,
//! `CHELABASE_ZITADEL_UPSTREAM`, `CHELABASE_ZITADEL_ADMIN_PAT_FILE`,
//! `CHELABASE_METADATA_DATABASE_URL` (its password from
//! `CHELABASE_METADATA_DATABASE_PASSWORD_FILE` when set), `CHELABASE_ORCHESTRATOR_HEALTH_URL`
//! (default `http://orchestrator:8083/health`; `none` skips the probe),
//! `CHELABASE_ZITADEL_ACTIONS_ENDPOINT`, and `CHELABASE_SMTP_HOST|PORT|USER|PASSWORD_FILE|FROM`.

pub mod outputs;
pub mod requests;
pub mod retry;
pub mod steps;
pub mod tenant;
pub mod zitadel;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result, bail};
use chelabase_types::mode::Mode;
use serde_json::json;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

use outputs::Outputs;
use retry::{Backoff, Check, Waiter};
use tenant::Metadata;
use zitadel::{ZitadelApi, api_error};

/// Whether the workbench's dev objects are made too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Demo {
    Off,
    On,
}

/// The tenant the metadata migrations seed, which the bootstrap binds.
pub const SEEDED_TENANT: Uuid = chelabase_metadata::DEMO_TENANT_ID;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);
pub const DEFAULT_ORCHESTRATOR_HEALTH: &str = "http://orchestrator:8083/health";
pub const DEFAULT_ACTIONS_ENDPOINT: &str = "http://orchestrator:8083/internal/zitadel/events";

/// Every environment variable the bootstrap reads.
pub const ENV_KEYS: &[&str] = &[
    "CHELABASE_PUBLIC_SCHEME",
    "CHELABASE_STUDIO_HOST",
    "CHELABASE_API_HOST",
    "CHELABASE_AUTH_HOST",
    "CHELABASE_DOCS_HOST",
    "CHELABASE_LANDING_HOST",
    "CHELABASE_MAIL_HOST",
    "CHELABASE_ZITADEL_ISSUER",
    "CHELABASE_MODE",
    "CHELABASE_ZITADEL_UPSTREAM",
    "CHELABASE_ZITADEL_ADMIN_PAT_FILE",
    "CHELABASE_METADATA_DATABASE_URL",
    "CHELABASE_METADATA_DATABASE_PASSWORD_FILE",
    "CHELABASE_ORCHESTRATOR_HEALTH_URL",
    "CHELABASE_ZITADEL_ACTIONS_ENDPOINT",
    "CHELABASE_SMTP_HOST",
    "CHELABASE_SMTP_PORT",
    "CHELABASE_SMTP_USER",
    "CHELABASE_SMTP_PASSWORD_FILE",
    "CHELABASE_SMTP_FROM",
];

/// The PAT Zitadel just wrote may not be accepted for a few seconds: a 401 on the first call
/// is waited on this long at most, never until the run's deadline.
const PAT_GRACE: Duration = Duration::from_secs(60);

/// One run's settings. No `Debug`: the metadata URL may carry its password.
pub struct Config {
    pub out_dir: PathBuf,
    pub zitadel_upstream: String,
    /// The `Host` Zitadel is addressed by (it picks its instance by it).
    pub auth_host: String,
    pub studio_origin: String,
    pub mode: Mode,
    pub admin_pat_file: PathBuf,
    pub metadata_url: String,
    /// `None`: no orchestrator probe (the metadata migrations and the Actions target still wait).
    pub orchestrator_health: Option<String>,
    pub actions_endpoint: String,
    pub smtp: Option<steps::Smtp>,
    /// `--demo --admin-email`: the tenant admin asked for.
    pub admin_email: Option<String>,
    pub tenant: Uuid,
    pub timeout: Duration,
    pub backoff: Backoff,
}

fn non_blank(get: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn read_secret_file(name: &str, path: &str) -> Result<String> {
    let text = std::fs::read_to_string(path).with_context(|| format!("{name}: reading {path}"))?;
    let value = text.trim_end_matches(['\r', '\n']).to_string();
    if value.trim().is_empty() {
        bail!("{name}: {path} is empty");
    }
    Ok(value)
}

/// `url` with `password` put in its user info (`postgres://user@…` -> `postgres://user:pw@…`).
pub fn with_password(url: &str, password: &str) -> Result<String> {
    let (scheme, rest) = url
        .split_once("://")
        .context("CHELABASE_METADATA_DATABASE_URL is not a URL")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let Some(at) = rest[..authority_end].rfind('@') else {
        bail!("CHELABASE_METADATA_DATABASE_URL names no user for its password file");
    };
    let user = &rest[..at];
    if user.contains(':') {
        bail!(
            "CHELABASE_METADATA_DATABASE_URL carries a password; with \
             CHELABASE_METADATA_DATABASE_PASSWORD_FILE it must not"
        );
    }
    Ok(format!(
        "{scheme}://{user}:{}{}",
        urlencoding::encode(password),
        &rest[at..]
    ))
}

/// SMTP from `CHELABASE_SMTP_*`, when `CHELABASE_SMTP_HOST` is set: TLS on, the password from
/// a file only.
fn smtp_from(get: &impl Fn(&str) -> Option<String>) -> Result<Option<steps::Smtp>> {
    if non_blank(get, "CHELABASE_SMTP_PASSWORD").is_some() {
        bail!(
            "CHELABASE_SMTP_PASSWORD isn't read: put the password in a file and set CHELABASE_SMTP_PASSWORD_FILE"
        );
    }
    let Some(host) = non_blank(get, "CHELABASE_SMTP_HOST") else {
        return Ok(None);
    };
    let port: u16 = match non_blank(get, "CHELABASE_SMTP_PORT") {
        None => 587,
        Some(p) => p
            .parse()
            .with_context(|| format!("CHELABASE_SMTP_PORT {p:?} is not a port"))?,
    };
    let from = non_blank(get, "CHELABASE_SMTP_FROM")
        .context("CHELABASE_SMTP_HOST is set without CHELABASE_SMTP_FROM (the sender address)")?;
    let user = non_blank(get, "CHELABASE_SMTP_USER");
    let password = match non_blank(get, "CHELABASE_SMTP_PASSWORD_FILE") {
        Some(path) => Some(read_secret_file("CHELABASE_SMTP_PASSWORD_FILE", &path)?),
        None => None,
    };
    if user.is_some() != password.is_some() {
        bail!("set both CHELABASE_SMTP_USER and CHELABASE_SMTP_PASSWORD_FILE, or neither");
    }
    let addr = format!("{host}:{port}");
    Ok(Some(steps::Smtp {
        payload: json!({"senderAddress": from, "senderName": "Chelabase", "tls": true,
            "host": addr, "user": user.unwrap_or_default(), "password": password.unwrap_or_default(),
            "description": "chelabase"}),
        host: addr,
    }))
}

impl Config {
    /// The settings from the environment (`get`), writing into `out_dir`; `--timeout` and
    /// `--admin-email` are set by the caller.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>, out_dir: PathBuf) -> Result<Self> {
        let entry = chelabase_types::entry::Entry::from_lookup(&get)?;
        let mode = Mode::parse(get(Mode::ENV).as_deref()).map_err(anyhow::Error::msg)?;
        let admin_pat_file = non_blank(&get, "CHELABASE_ZITADEL_ADMIN_PAT_FILE")
            .context("CHELABASE_ZITADEL_ADMIN_PAT_FILE is required (Zitadel's admin PAT)")?;
        let url = non_blank(&get, "CHELABASE_METADATA_DATABASE_URL")
            .context("CHELABASE_METADATA_DATABASE_URL is required")?;
        let metadata_url = match non_blank(&get, "CHELABASE_METADATA_DATABASE_PASSWORD_FILE") {
            Some(path) => with_password(
                &url,
                &read_secret_file("CHELABASE_METADATA_DATABASE_PASSWORD_FILE", &path)?,
            )?,
            None => url,
        };
        let orchestrator_health = match non_blank(&get, "CHELABASE_ORCHESTRATOR_HEALTH_URL") {
            Some(v) if v.eq_ignore_ascii_case("none") => None,
            Some(v) => Some(v),
            None => Some(DEFAULT_ORCHESTRATOR_HEALTH.to_string()),
        };
        Ok(Self {
            out_dir,
            zitadel_upstream: chelabase_types::entry::zitadel_upstream(&get),
            auth_host: entry.auth_host().to_string(),
            studio_origin: entry.studio_origin(),
            mode,
            admin_pat_file: admin_pat_file.into(),
            metadata_url,
            orchestrator_health,
            actions_endpoint: non_blank(&get, "CHELABASE_ZITADEL_ACTIONS_ENDPOINT")
                .unwrap_or_else(|| DEFAULT_ACTIONS_ENDPOINT.to_string()),
            smtp: smtp_from(&get)?,
            admin_email: None,
            tenant: SEEDED_TENANT,
            timeout: DEFAULT_TIMEOUT,
            backoff: Backoff::default(),
        })
    }

    fn auth_host_name(&self) -> String {
        chelabase_types::entry::normalize_host(&self.auth_host)
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
    http: reqwest::Client,
    url: String,
    what: String,
}

impl HttpProbe {
    pub fn new(what: &str, url: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
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
            Ok(r) if r.status().as_u16() == 404 || r.status().is_server_error() => {
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

/// Waits for the admin PAT file (Zitadel writes it once it has set up its instance).
async fn wait_for_pat(w: &Waiter, path: &Path) -> Result<String> {
    w.until(
        &format!("Zitadel's admin PAT {}", path.display()),
        || async {
            Ok(match std::fs::read_to_string(path) {
                Ok(text) => {
                    let token: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                    if token.is_empty() {
                        Check::Wait("the file is empty".into())
                    } else {
                        Check::Ready(token)
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    Check::Wait("not written yet".into())
                }
                Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
            })
        },
    )
    .await
}

/// `chelabase-init bootstrap`: the real Zitadel, metadata database and orchestrator.
pub async fn run(cfg: &Config, demo: Demo) -> Result<()> {
    let w = Waiter::new(cfg.timeout, cfg.backoff);
    let pat = wait_for_pat(&w, &cfg.admin_pat_file).await?;
    let api = zitadel::HttpZitadel::via_upstream(&cfg.zitadel_upstream, &cfg.auth_host, &pat);
    let metadata = tenant::SqlMetadata::new(&cfg.metadata_url)?;
    match &cfg.orchestrator_health {
        Some(url) => {
            run_with(
                cfg,
                demo,
                &api,
                &metadata,
                &HttpProbe::new("the orchestrator", url),
            )
            .await
        }
        None => run_with(cfg, demo, &api, &metadata, &NoProbe).await,
    }
}

/// A Zitadel call's error as a readiness answer: unavailable is "not yet", anything else fails.
fn unless_unavailable<T>(result: Result<T>) -> Result<Check<T>> {
    match result {
        Ok(v) => Ok(Check::Ready(v)),
        Err(e) if api_error(&e).is_some_and(|a| a.unavailable()) => {
            Ok(Check::Wait(format!("{e:#}")))
        }
        Err(e) => Err(e),
    }
}

/// One Zitadel step: retried only while Zitadel is unavailable; any other failure names it.
async fn zitadel_step<T, F, Fut>(w: &Waiter, step: &str, call: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut call = call;
    w.until(&format!("Zitadel ({step})"), || {
        let fut = call();
        async move { unless_unavailable(fut.await) }
    })
    .await
    .with_context(|| format!("bootstrap step: {step}"))
}

/// Zitadel ready: its probe, then the first authenticated call (the default org). A 401 there
/// is waited on for [`PAT_GRACE`] at most (a PAT Zitadel has just written).
async fn wait_for_zitadel(w: &Waiter, api: &impl ZitadelApi) -> Result<(String, Option<String>)> {
    w.until("Zitadel", || async {
        Ok(match api.ready().await {
            Ok(()) => Check::Ready(()),
            Err(e) if e.unavailable() || e.status == Some(404) => Check::Wait(e.to_string()),
            Err(e) => return Err(e.into()),
        })
    })
    .await?;
    w.at_most(PAT_GRACE)
        .until("Zitadel to accept the admin PAT", || async {
            match steps::ensure_org(api).await {
                Err(e) if api_error(&e).is_some_and(|a| a.status == Some(401)) => {
                    Ok(Check::Wait(format!("{e:#}")))
                }
                other => unless_unavailable(other),
            }
        })
        .await
        .context("bootstrap step: the default org")
}

/// The ids the apps read, written once the tenant is bound to their org.
struct Ids {
    org: String,
    project: String,
    api_client: String,
    studio_client: String,
    demo_user: Option<String>,
}

fn write_ids(out: &Outputs, ids: &Ids) -> Result<()> {
    for (name, value) in [
        (outputs::ORG_ID, Some(&ids.org)),
        (outputs::PROJECT_ID, Some(&ids.project)),
        (outputs::API_CLIENT_ID, Some(&ids.api_client)),
        (outputs::STUDIO_CLIENT_ID, Some(&ids.studio_client)),
        (outputs::DEMO_USER_ID, ids.demo_user.as_ref()),
    ] {
        if let Some(v) = value
            && out.write_value(name, v)?
        {
            info!("wrote {}", out.path().join(name).display());
        }
    }
    Ok(())
}

/// `--demo`: the tenant admin asked for (it must exist), else Zitadel's first human (a
/// warning when it doesn't). Looked up before the bind: a bind must not be left without the
/// admin that was asked for.
async fn find_demo_admin(
    w: &Waiter,
    cfg: &Config,
    api: &impl ZitadelApi,
    org: &str,
) -> Result<Option<(String, String)>> {
    let requested = cfg
        .admin_email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(chelabase_zitadel::rules::normalize_email);
    let candidates = match &requested {
        Some(email) => vec![email.clone()],
        None => steps::first_human_usernames(&cfg.auth_host_name())
            .into_iter()
            .filter(|n| n.contains('@'))
            .collect(),
    };
    for candidate in &candidates {
        let found = zitadel_step(w, "the tenant admin's user", || {
            steps::find_user_by_email(api, org, candidate)
        })
        .await?;
        if let Some(id) = found {
            return Ok(Some((id, candidate.clone())));
        }
    }
    if requested.is_some() {
        bail!(
            "no Zitadel user with email {} in the org: create that user in Zitadel first",
            candidates[0]
        );
    }
    warn!(
        "dev user not found in Zitadel (tried {}): no tenant admin is set, so the first \
         person to sign in to Studio claims it. Re-run with --admin-email before exposing Studio",
        candidates.join(", ")
    );
    Ok(None)
}

/// [`run`] with its dependencies given.
pub async fn run_with(
    cfg: &Config,
    demo: Demo,
    api: &impl ZitadelApi,
    metadata: &impl Metadata,
    orchestrator: &impl Probe,
) -> Result<()> {
    // Every output name is checked before anything else happens.
    let out = Outputs::open(&cfg.out_dir)?;
    let _lock = out.lock()?;
    let w = Waiter::new(cfg.timeout, cfg.backoff);

    let (org, org_name) = wait_for_zitadel(&w, api).await?;
    let usernames = steps::first_human_usernames(&cfg.auth_host_name());
    if demo == Demo::Off {
        zitadel_step(&w, "deactivating Zitadel's first human user", || {
            steps::deactivate_first_human(api, &usernames)
        })
        .await?;
    }

    let project = zitadel_step(&w, "the project", || steps::ensure_project(api, &org)).await?;
    let api_client =
        zitadel_step(&w, "the API app", || steps::ensure_api_app(api, &project)).await?;
    let studio_client = zitadel_step(&w, "the Studio app", || {
        steps::ensure_studio_app(api, &project, cfg.mode, &cfg.studio_origin)
    })
    .await?;
    zitadel_step(&w, "Studio's login", || {
        steps::ensure_studio_login(api, &project, cfg.mode, &cfg.studio_origin)
    })
    .await?;
    zitadel_step(&w, "Zitadel's own registration off", || {
        steps::ensure_zitadel_registration_off(api)
    })
    .await?;

    let demo_user = match demo {
        Demo::On => {
            let id = zitadel_step(&w, "the demo machine user", || {
                steps::ensure_demo_user(api, &org)
            })
            .await?;
            zitadel_step(&w, "the demo machine user's key", || {
                steps::ensure_demo_key(api, &id, &out)
            })
            .await?;
            Some(id)
        }
        Demo::Off => None,
    };
    let smtp = match (&cfg.smtp, demo) {
        (Some(smtp), _) => Some(smtp.clone()),
        (None, Demo::On) => Some(steps::Smtp::mailpit()),
        (None, Demo::Off) => None,
    };
    if let Some(smtp) = &smtp {
        zitadel_step(&w, "SMTP", || steps::ensure_smtp(api, smtp)).await?;
    }
    let admin = match demo {
        Demo::On => find_demo_admin(&w, cfg, api, &org).await?,
        Demo::Off => None,
    };

    // The orchestrator migrates the metadata database and receives the Actions events.
    w.until("the metadata database's migrations", || metadata.migrated())
        .await?;
    w.until(&orchestrator.name(), || orchestrator.check())
        .await?;

    let tenant = cfg.tenant;
    if metadata
        .bind_org(tenant, &org)
        .await
        .context("bootstrap step: the tenant bind")?
    {
        info!("bound tenant {tenant} to Zitadel org {org}");
    }
    if let Some(name) = org_name.as_deref().and_then(tenant::tenant_name_from_org)
        && metadata
            .fill_display_name(tenant, &name)
            .await
            .context("bootstrap step: the tenant's name")?
    {
        info!("named tenant {tenant} {name:?} after the Zitadel org");
    }
    if let Some((user, email)) = &admin {
        metadata
            .bootstrap_admin(tenant, user, Some(email))
            .await
            .context("bootstrap step: the tenant admin")?;
        info!("tenant {tenant}: {email} is an active admin (member {user})");
    }
    // Every `dev token` script acts as the demo machine user, so it is an admin too
    // (self-hosted mode only). It has no email.
    if let Some(user) = &demo_user
        && cfg.mode == Mode::SelfHosted
    {
        metadata
            .bootstrap_admin(tenant, user, None)
            .await
            .context("bootstrap step: the demo machine user's admin")?;
        info!(
            "tenant {tenant}: the demo machine user {} is an active admin (member {user}, no email)",
            steps::DEMO_USER
        );
    }
    write_ids(
        &out,
        &Ids {
            org: org.clone(),
            project,
            api_client,
            studio_client,
            demo_user,
        },
    )?;

    // Last: Zitadel refuses a target whose host (the orchestrator) doesn't resolve yet.
    w.until(
        "Zitadel (the Actions target; its host must resolve)",
        || async {
            match steps::ensure_actions_target(api, &out, &cfg.actions_endpoint).await {
                Err(e) if api_error(&e).is_some_and(|a| a.body.contains("DeniedURL")) => {
                    Ok(Check::Wait(format!("{e:#}")))
                }
                other => unless_unavailable(other),
            }
        },
    )
    .await
    .context("bootstrap step: the Actions target")?;

    info!("bootstrap complete");
    Ok(())
}
