//! The `chelabase-init` subcommands, writing to the given streams (the binary passes stdout and
//! stderr). Only the setup code is ever printed, and only by the run that creates it or by
//! `setup-code`; every other line names files, never their contents.

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use crate::secrets::{InitSecrets, ensure_secrets, read_setup_code};

/// `chelabase-init secrets --dir <dir>`.
pub fn secrets(
    dir: &Path,
    opts: &InitSecrets,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    let report = ensure_secrets(dir, opts)?;
    writeln!(
        err,
        "chelabase-init: {} created, {} re-rendered, in {}",
        report.created.len(),
        report.rewritten.len(),
        dir.display()
    )?;
    if let Some(code) = &report.setup_code {
        writeln!(out, "Setup code: {code}")?;
    }
    out.flush()?;
    Ok(())
}

/// `chelabase-init setup-code --dir <dir>`: the stored code and nothing else.
pub fn setup_code(dir: &Path, out: &mut dyn Write) -> Result<()> {
    writeln!(out, "{}", read_setup_code(dir)?)?;
    out.flush()?;
    Ok(())
}
