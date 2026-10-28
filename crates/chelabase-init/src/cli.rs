//! The `chelabase-init` subcommands, writing to the given streams (the binary passes stdout and
//! stderr). Only the setup code is ever printed, and only by the run that creates it or by
//! `setup-code`; every other line names files, never their contents.

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use crate::secrets::{InitSecrets, Report, ensure_secrets_into, read_setup_code};

/// `chelabase-init secrets --dir <dir>`. A setup code the run created is printed even when a
/// later step fails (it is written last, so none should).
pub fn secrets(
    dir: &Path,
    opts: &InitSecrets,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    let mut report = Report::default();
    let result = ensure_secrets_into(dir, opts, &mut report);
    if let Some(code) = &report.setup_code {
        writeln!(out, "Setup code: {code}")?;
        out.flush()?;
    }
    result?;
    writeln!(
        err,
        "chelabase-init: {} created, {} re-rendered, in {}",
        report.created.len(),
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
