//! Cheladb local CLI — auth bootstrap / token mint (foundation for a fuller control-plane CLI).

mod auth;
mod paths;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "cheladb", about = "Cheladb local control-plane CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Local Zitadel IAM helpers
    Auth {
        #[command(subcommand)]
        command: AuthCommands,
    },
}

#[derive(Subcommand, Debug)]
enum AuthCommands {
    /// Idempotent ensure: project, API app, demo machine key, tenant org bind
    Bootstrap,
    /// Mint a short-lived JWT access token (stdout) via JWT profile
    Token,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cheladb=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Auth { command } => match command {
            AuthCommands::Bootstrap => auth::bootstrap::run().await,
            AuthCommands::Token => auth::token::run().await,
        },
    }
}
