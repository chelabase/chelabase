//! `chelabase-init example`: seeds a fresh, local instance with the Acme demo people, through
//! the public endpoints only.
//!
//! 1. [`gate::check`] refuses anything that isn't a local stack, before any request.
//! 2. Ada (`ada@acme.example`) claims the instance with the setup code through the ordinary
//!    sign-up (or, once it is claimed, signs in), and must then be its tenant admin.
//! 3. If an earlier run finished (the completion marker, [`acme::complete`]), it stops here:
//!    no invite, rename, grant or schema change, whatever the demo looks like now.
//! 4. Ada invites each demo person with link delivery, and the seed accepts each invite with
//!    the demo password ([`people`]).
//! 5. Ada builds the Acme organisation: teams, the project and database, schemas, RLS, app
//!    users, a key, branches and saved queries, then writes the marker ([`acme`]).
//!
//! Until the marker is written every step is idempotent, so a run that stopped anywhere is
//! finished by the next one. The demo password is a constant, never configured, so it is the
//! same on every run. Nothing printed carries a code, token or the setup code ([`session`]).

pub mod acme;
pub mod gate;
pub mod people;
pub mod session;

#[cfg(test)]
mod fake;

use anyhow::{Result, anyhow};
use chelabase_types::entry::Entry;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

/// Every demo account's password: local demo only.
pub const DEMO_PASSWORD: &str = "Acme-Demo-2026!";

/// The seed's settings, from the environment.
pub struct Config {
    pub entry: Entry,
    /// The gateway service, as the seed reaches it (`http://gateway:8000`).
    pub gateway: String,
    pub setup_code_file: PathBuf,
    pub http_addr: Option<String>,
    pub proxy_addr: Option<String>,
    pub require_email_verification: Option<String>,
    pub timeout: Duration,
}

impl Config {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let entry = Entry::from_lookup(&get).map_err(|e| anyhow!("{}", e.0))?;
        let need = |k: &str| {
            get(k)
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| anyhow!("{k} is not set"))
        };
        Ok(Self {
            entry,
            gateway: need("CHELABASE_EXAMPLE_GATEWAY_URL")?,
            setup_code_file: need("CHELABASE_SETUP_CODE_FILE")?.into(),
            http_addr: get("CHELABASE_HTTP_ADDR"),
            proxy_addr: get("CHELABASE_PROXY_ADDR"),
            require_email_verification: get("CHELABASE_REQUIRE_EMAIL_VERIFICATION"),
            timeout: Duration::from_secs(600),
        })
    }
}

/// The whole seed: the gate, then the claim and the people, logging to `out`.
pub async fn run(cfg: &Config, http: &impl session::Http, out: &mut dyn Write) -> Result<()> {
    gate::check(cfg)?;
    let (session, ada, writes) = people::claim(cfg, http, Duration::from_secs(1), out).await?;
    if acme::complete(&session, &ada).await? {
        writeln!(
            out,
            "example: the seed is already complete on this instance: skipped, 0 writes. Changes made since are kept; reset with `down -v` to seed again"
        )?;
        return people::roster(out);
    }
    people::admit(&session, &ada, writes, out).await?;
    acme::seed(&session, &ada, out).await?;
    Ok(())
}

/// `chelabase-init example`, configured from the environment.
pub fn cli(out: &mut dyn Write) -> Result<()> {
    let cfg = Config::from_lookup(|k| std::env::var(k).ok())?;
    let http = session::Gateway::new(&cfg.gateway, cfg.entry.clone())?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(&cfg, &http, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_demo_password_passes_the_sign_up_rule() {
        assert!(chelabase_identity::rules::strong_password(DEMO_PASSWORD));
    }

    /// Fails each request in turn, with an answer stuffed with secrets, and scans what the
    /// binary would print: the log lines and the error.
    #[tokio::test]
    async fn no_secret_reaches_the_output_at_any_failure() {
        let clean = fake::Fake::new();
        let mut sink = Vec::new();
        run(&fake::config(), &clean, &mut sink).await.unwrap();
        let total = clean.calls();
        assert!(total > 10, "{total}");
        for n in 0..total {
            let f = fake::Fake::new();
            f.fail_at(n);
            let mut out = Vec::new();
            let err = run(&fake::config(), &f, &mut out)
                .await
                .expect_err("the forced failure fails the run");
            let text = format!(
                "{}\nchelabase-init: {err:#}\n{err:?}",
                String::from_utf8_lossy(&out)
            );
            for secret in f.secrets() {
                assert!(
                    !text.contains(&secret),
                    "call {n} leaked {secret:?}:\n{text}"
                );
            }
            for marker in ["code=", "t=", "Bearer", "eyJ"] {
                assert!(
                    !text.contains(marker),
                    "call {n} printed {marker:?}:\n{text}"
                );
            }
        }
    }
}
