//! `chelabase-init`: a self-hosted stack's one-shot setup.
//!
//! - `chelabase-init secrets --dir <dir> [--owners <file>]` makes every key, secret and auth file
//!   the stack's services read (never overwriting one) and prints `Setup code: …` on the run
//!   that creates the setup code. It prints no other secret.
//! - `chelabase-init setup-code --dir <dir>` prints the stored setup code again.

use anyhow::Context;
use chelabase_init::owners::OwnershipMap;
use chelabase_init::{InitSecrets, cli};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "chelabase-init",
    version,
    about = "A Chelabase stack's one-shot setup"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create every missing key, secret and auth file under the directory.
    Secrets {
        /// The secrets directory (created 0700 if missing).
        #[arg(long)]
        dir: PathBuf,
        /// A JSON file overriding the default owners and modes.
        #[arg(long)]
        owners: Option<PathBuf>,
    },
    /// Print the stored setup code.
    SetupCode {
        /// The secrets directory.
        #[arg(long)]
        dir: PathBuf,
    },
}

fn run(args: Args) -> anyhow::Result<()> {
    let (mut out, mut err) = (std::io::stdout().lock(), std::io::stderr().lock());
    match args.command {
        Command::Secrets { dir, owners } => {
            let map = match owners {
                Some(path) => {
                    let text = std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    OwnershipMap::from_override_json(&text)
                        .with_context(|| format!("in {}", path.display()))?
                }
                None => OwnershipMap::default(),
            };
            cli::secrets(&dir, &InitSecrets::self_hosted(map), &mut out, &mut err)
        }
        Command::SetupCode { dir } => cli::setup_code(&dir, &mut out),
    }
}

fn main() -> std::process::ExitCode {
    match run(Args::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("chelabase-init: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
