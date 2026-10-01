//! Cheladb local CLI — local development setup / token mint (foundation for a fuller control-plane CLI).

mod dev;
mod generate;
mod neon;
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
    /// Local development setup: Zitadel, keys and env files
    Dev {
        #[command(subcommand)]
        command: DevCommands,
    },
    /// Neon compute helpers (roadmap step 3)
    Neon {
        #[command(subcommand)]
        command: NeonCommands,
    },
    /// Generate code from a database's data API schema
    Gen {
        #[command(subcommand)]
        command: GenCommands,
    },
}

#[derive(Subcommand, Debug)]
enum GenCommands {
    /// Write the TypeScript `Database` and runtime `types` for `@cheladb/client`
    Types {
        /// The gateway's base URL (the data API is under `/data/v1`)
        #[arg(long)]
        url: String,
        /// A secret data key (`chk_sec_…`)
        #[arg(long, env = "CHELADB_DATA_KEY", hide_env_values = true)]
        key: Option<String>,
        /// Write to this file instead of stdout
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum DevCommands {
    /// Idempotent ensure: project, API app, demo machine key, tenant org bind
    Bootstrap,
    /// Mint a short-lived JWT access token (stdout) via JWT profile
    Token,
}

#[derive(Subcommand, Debug)]
enum NeonCommands {
    /// Print the SCRAM-SHA-256 verifier for the password on stdin
    Verifier,
    /// Print compute_ctl's arguments for an endpoint, one per line
    ComputeArgs {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        control_plane_uri: String,
    },
    /// Tell the proxies to drop a role's cached secret (after a password change)
    PasswordUpdated {
        #[arg(long)]
        endpoint: String,
        #[arg(long)]
        role: String,
    },
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
        Commands::Dev { command } => match command {
            DevCommands::Bootstrap => dev::bootstrap::run().await,
            DevCommands::Token => dev::token::run().await,
        },
        Commands::Gen { command } => match command {
            GenCommands::Types { url, key, out } => {
                generate::run(&url, key, out.as_deref()).await
            }
        },
        Commands::Neon { command } => match command {
            NeonCommands::Verifier => neon::verifier(),
            NeonCommands::ComputeArgs {
                endpoint,
                control_plane_uri,
            } => {
                neon::compute_args(&endpoint, &control_plane_uri);
                Ok(())
            }
            NeonCommands::PasswordUpdated { endpoint, role } => {
                neon::password_updated(&endpoint, &role).await
            }
        },
    }
}
