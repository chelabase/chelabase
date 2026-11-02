//! The bootstrap's identity step (auth-replacement m5, #118): the identity service's database
//! roles and functions ([`crate::roles`]), then the `members` pool through the service's own
//! route (`POST /internal/v1/pools/members`, idempotent: it creates the schema through
//! `identity_admin.create_pool_schema` and runs Better Auth's migrations, both no-ops on a pool
//! that is complete, and completes a half-made one).
//!
//! The bootstrap calls it with its own token (`tokens/bootstrap-identity.token`), which the
//! service lets through to that one route only (`services/identity/src/routes.ts`), so the
//! service tells the bootstrap from the worker and the worker's token stays with the worker.
//!
//! Environment ([`ENV_KEYS`]): `CHELABASE_IDENTITY_URL` (the service, e.g.
//! `http://identity:8080`), `CHELABASE_IDENTITY_TOKEN_FILE` (the bootstrap's
//! `tokens/bootstrap-identity.token`) and `CHELABASE_IDENTITY_DB_PASSWORD_FILE`
//! (`identity/db.password`, the `chelabase_identity` role's password). All three are required.

use anyhow::{Context, Result, bail};
use sqlx::Connection;
use std::time::Duration;
use tracing::info;

use super::retry::{Check, Waiter};
use super::{HttpProbe, Probe, non_blank, read_secret_file};
use crate::roles::{IdentityRoles, ensure_identity_roles, is_lock_timeout};

/// Every environment variable this step reads.
pub const ENV_KEYS: &[&str] = &[
    "CHELABASE_IDENTITY_URL",
    "CHELABASE_IDENTITY_TOKEN_FILE",
    "CHELABASE_IDENTITY_DB_PASSWORD_FILE",
];

/// The pool every install has: the platform's members.
pub const MEMBERS_POOL: &str = "members";

/// The step's settings. No `Debug`: it holds a token and a password.
pub struct IdentityConfig {
    /// The service's base URL, without a trailing `/`.
    pub url: String,
    token: String,
    db_password: String,
}

impl IdentityConfig {
    /// From the environment (`get`); every key is required.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let url = non_blank(&get, "CHELABASE_IDENTITY_URL")
            .context("CHELABASE_IDENTITY_URL is required (the identity service)")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("CHELABASE_IDENTITY_URL must be an http(s) URL");
        }
        let token_file = non_blank(&get, "CHELABASE_IDENTITY_TOKEN_FILE").context(
            "CHELABASE_IDENTITY_TOKEN_FILE is required (the bootstrap's identity token)",
        )?;
        let password_file = non_blank(&get, "CHELABASE_IDENTITY_DB_PASSWORD_FILE").context(
            "CHELABASE_IDENTITY_DB_PASSWORD_FILE is required (the identity role's password)",
        )?;
        Ok(Self {
            url: url.trim_end_matches('/').to_string(),
            token: read_secret_file("CHELABASE_IDENTITY_TOKEN_FILE", &token_file)?,
            db_password: read_secret_file("CHELABASE_IDENTITY_DB_PASSWORD_FILE", &password_file)?,
        })
    }

    /// Settings given directly (the live tests).
    pub fn new(url: &str, token: &str, db_password: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            token: token.trim().to_string(),
            db_password: db_password.trim().to_string(),
        }
    }
}

/// The roles and functions in the metadata database, as its superuser (`metadata_url`); waits
/// while the database isn't up, and while another session holds the setup's lock (each attempt
/// gives up on it after [`crate::roles::LOCK_TIMEOUT`]), both within `w`'s deadline.
pub async fn ensure_roles(
    w: &Waiter,
    metadata_url: &str,
    roles: &IdentityRoles,
    password: &str,
) -> Result<()> {
    w.until("the metadata database (identity roles)", || async {
        let mut conn = match sqlx::PgConnection::connect(metadata_url).await {
            Ok(c) => c,
            Err(e) => {
                return match super::tenant::sql_not_ready(&e) {
                    Some(reason) => Ok(Check::Wait(reason)),
                    None => {
                        Err(anyhow::Error::new(e).context("connecting to the metadata database"))
                    }
                };
            }
        };
        let result = ensure_identity_roles(&mut conn, roles, password).await;
        let _ = conn.close().await;
        match result {
            Ok(()) => Ok(Check::Ready(())),
            Err(e) if is_lock_timeout(&e) => Ok(Check::Wait(
                "another session holds the identity roles' setup lock".into(),
            )),
            Err(e) => Err(e.context("bootstrap step: the identity roles")),
        }
    })
    .await
}

/// One `POST /internal/v1/pools/<pool>`: `Ready` on a 2xx, `Wait` while the service isn't
/// answering (no connection, 502-504), an error otherwise (naming the status, never the token).
pub async fn create_pool(
    http: &chelabase_http::Client,
    cfg: &IdentityConfig,
    pool: &str,
) -> Result<Check<()>> {
    let url = format!("{}/internal/v1/pools/{pool}", cfg.url);
    match http.post(&url).bearer_auth(&cfg.token).send().await {
        Err(e) => Ok(Check::Wait(format!("no answer ({e})"))),
        Ok(r) if r.status().is_success() => Ok(Check::Ready(())),
        Ok(r) if matches!(r.status().as_u16(), 502..=504) => {
            Ok(Check::Wait(format!("it answers {}", r.status())))
        }
        Ok(r) => {
            let status = r.status();
            let body: String = r
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            bail!("the identity service answers {status} to POST {url}: {body}")
        }
    }
}

/// The whole step: the roles, the service's health, then the `members` pool.
pub async fn run(w: &Waiter, metadata_url: &str, cfg: &IdentityConfig) -> Result<()> {
    ensure_roles(w, metadata_url, &IdentityRoles::default(), &cfg.db_password).await?;
    info!("identity: the chelabase_identity role and identity_admin functions are in place");
    let health = HttpProbe::new("the identity service", &format!("{}/health", cfg.url));
    w.until(&health.name(), || health.check()).await?;
    let http = chelabase_http::client_builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("the identity client")?;
    w.until("the identity service (the members pool)", || {
        create_pool(&http, cfg, MEMBERS_POOL)
    })
    .await
    .context("bootstrap step: the members pool")?;
    info!("identity: the members pool is ready");
    Ok(())
}
