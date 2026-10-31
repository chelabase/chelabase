//! `chelabase-init`: a self-hosted stack's one-shot setup.
//!
//! - `chelabase-init secrets --dir <dir> [--owners <file>] [--seeds <file>]` makes every key,
//!   secret and auth file the stack's services read (never overwriting one) and prints
//!   `Setup code: …` on the run that creates the setup code. It prints no other secret.
//!   `--seeds` reads chosen values from a dotenv file (the compose `.env`): each
//!   `CHELABASE_SEED_*` line is written as its file, instead of a generated value, on the run
//!   that creates that file (see `chelabase_init::seed`).
//! - `chelabase-init setup-code --dir <dir>` prints the stored setup code again.
//! - `chelabase-init bootstrap --out <dir> [--demo]` makes the product's Zitadel objects and
//!   binds the seeded tenant, waiting for Zitadel, the metadata migrations and the orchestrator
//!   (see `chelabase_init::bootstrap`). It writes only into `--out` and creates no key.
//! - `chelabase-init example` claims a fresh local instance as the demo tenant admin and adds
//!   the demo people, through the public endpoints only (see `chelabase_init::example`). It
//!   refuses any stack that isn't local, and prints no code, token or setup code.

use anyhow::Context;
use chelabase_init::owners::OwnershipMap;
use chelabase_init::bootstrap::Demo;
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
        /// A dotenv file (the compose .env) whose CHELABASE_SEED_* lines give chosen values for
        /// the files this run creates. A file that already exists with another value fails the
        /// run. Values are never printed.
        #[arg(long)]
        seeds: Option<PathBuf>,
    },
    /// Make the product's Zitadel objects and bind the seeded tenant (idempotent).
    Bootstrap {
        /// The output dir: the only place it writes (org and client ids, the Actions key).
        #[arg(long)]
        out: PathBuf,
        /// Also make the workbench's dev objects: the demo machine user and its key, Mailpit
        /// as SMTP, the tenant admins. Keeps Zitadel's first human user active.
        #[arg(long)]
        demo: bool,
        /// With --demo: the tenant admin's email (a Zitadel user that must exist).
        #[arg(long, requires = "demo")]
        admin_email: Option<String>,
        /// How long to wait for Zitadel, the metadata migrations and the orchestrator
        /// (seconds, or 30s, 10m, 1h).
        #[arg(long, default_value = "10m", value_parser = parse_timeout)]
        timeout: std::time::Duration,
    },
    /// Seed a fresh local instance with the Acme demo people (local demo only; idempotent).
    /// Configured from the environment (the example compose overlay).
    Example,
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
        Command::Secrets { dir, owners, seeds } => {
            let map = match owners {
                Some(path) => {
                    let text = std::fs::read_to_string(&path)
                        .with_context(|| format!("reading {}", path.display()))?;
                    OwnershipMap::from_override_json(&text)
                        .with_context(|| format!("in {}", path.display()))?
                }
                None => OwnershipMap::default(),
            };
            cli::secrets(
                &dir,
                &InitSecrets::self_hosted(map),
                seeds.as_deref(),
                &mut out,
                &mut err,
            )
        }
        Command::SetupCode { dir } => cli::setup_code(&dir, &mut out),
        Command::Example => chelabase_init::example::cli(&mut err),
        Command::Bootstrap {
            out: dir,
            demo,
            admin_email,
            timeout,
        } => {
            drop((out, err));
            let demo = if demo { Demo::On } else { Demo::Off };
            cli::bootstrap(&dir, demo, admin_email, timeout)
        }
    }
}

fn parse_timeout(text: &str) -> Result<std::time::Duration, String> {
    chelabase_init::bootstrap::retry::parse_timeout(text).map_err(|e| e.to_string())
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
