//! The `chelabase-init` subcommands, writing to the given streams (the binary passes stdout and
//! stderr). Only the setup code is ever printed, and only by the run that creates it or by
//! `setup-code`; every other line names files, never their contents.

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use crate::secrets::{InitSecrets, Report, ensure_secrets_into, read_setup_code};
use crate::seed::Seeds;

/// `chelabase-init secrets --dir <dir> [--seeds <file>]`. With `seeds`, the file's
/// `CHELABASE_SEED_*` values ([`crate::seed`]) replace `opts.seeds`; it is read and checked
/// before anything is written, and a warning goes to `err` when its group or others can open it.
/// A setup code the run created is printed even when a later step fails (it is written last, so
/// none should). Seeded files are named on `err`, never their values.
pub fn secrets(
    dir: &Path,
    opts: &InitSecrets,
    seeds: Option<&Path>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    let mut opts = opts.clone();
    if let Some(path) = seeds {
        let loaded = Seeds::load(path)?;
        if let Some(warning) = &loaded.warning {
            writeln!(err, "chelabase-init: warning: {warning}")?;
        }
        opts.seeds = loaded.seeds;
    }
    let mut report = Report::default();
    let result = ensure_secrets_into(dir, &opts, &mut report);
    if let Some(code) = &report.setup_code {
        writeln!(out, "Setup code: {code}")?;
        out.flush()?;
    }
    result?;
    for path in &report.seeded {
        writeln!(err, "chelabase-init: seeded {}", path.display())?;
    }
    writeln!(
        err,
        "chelabase-init: {} created, {} seeded, {} kept, {} re-rendered, in {}",
        report.created.len(),
        report.seeded.len(),
        report.kept.len(),
        report.rewritten.len(),
        dir.display()
    )?;
    Ok(())
}

/// `chelabase-init setup-code --dir <dir>`: the stored code and nothing else.
pub fn setup_code(dir: &Path, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "{}", read_setup_code(dir)?)?;
    out.flush()?;
    Ok(())
}

/// `chelabase-init bootstrap --out <dir> [--demo] [--admin-email <email>] [--timeout <t>]`,
/// configured from the environment ([`crate::bootstrap::ENV_KEYS`]). Logs progress to stderr;
/// prints no secret.
pub fn bootstrap(
    out: &Path,
    demo: crate::bootstrap::Demo,
    admin_email: Option<String>,
    timeout: std::time::Duration,
) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "chelabase_init=info".into()),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let mut cfg =
        crate::bootstrap::Config::from_lookup(|k| std::env::var(k).ok(), out.to_path_buf())?;
    cfg.admin_email = admin_email;
    cfg.timeout = timeout;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(crate::bootstrap::run(&cfg, demo))
}
