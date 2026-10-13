//! Chelabase local CLI — local development setup / token mint (foundation for a fuller control-plane CLI).

mod dev;
mod generate;
mod neon;
mod paths;
mod workbench;

use clap::{Parser, Subcommand};
use workbench::commands::Site;

#[derive(Parser, Debug)]
#[command(name = "chelabase", about = "Chelabase local control-plane CLI", version)]
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
    /// This checkout's isolated stack: up, test, run, down (W2)
    Workbench {
        #[command(subcommand)]
        command: WorkbenchCommands,
    },
}

/// `chelabase workbench …`, as the commands use it. Clap parses [`WorkbenchCli`] (the same
/// commands, with `run`'s script and arguments as one trailing list) and converts.
#[derive(Debug)]
enum WorkbenchCommands {
    Up {
        with: Vec<Site>,
    },
    Restart {
        apps: Vec<String>,
    },
    Down {
        stack: Option<String>,
    },
    Purge {
        stack: Option<String>,
        yes: bool,
    },
    Status,
    Logs {
        follow: bool,
        services: Vec<String>,
    },
    Psql {
        db: Option<String>,
    },
    Exec {
        service: String,
        cmd: Vec<String>,
    },
    Test {
        archive: Vec<String>,
        run: Vec<String>,
    },
    Run {
        env: Vec<String>,
        script: String,
        args: Vec<String>,
    },
    Env,
}

impl From<WorkbenchCli> for WorkbenchCommands {
    fn from(cli: WorkbenchCli) -> Self {
        match cli {
            WorkbenchCli::Up { with } => Self::Up { with },
            WorkbenchCli::Restart { apps } => Self::Restart { apps },
            WorkbenchCli::Down { stack } => Self::Down { stack },
            WorkbenchCli::Purge { stack, yes } => Self::Purge { stack, yes },
            WorkbenchCli::Status => Self::Status,
            WorkbenchCli::Logs { follow, services } => Self::Logs { follow, services },
            WorkbenchCli::Psql { db } => Self::Psql { db },
            WorkbenchCli::Exec { service, cmd } => Self::Exec { service, cmd },
            WorkbenchCli::Test { archive, run } => Self::Test { archive, run },
            WorkbenchCli::Run { env, argv } => {
                let mut argv = argv.into_iter();
                // Clap requires at least one value.
                let script = argv.next().unwrap_or_default();
                let mut args: Vec<String> = argv.collect();
                // A `--` right after the script separates it from its arguments.
                if args.first().is_some_and(|a| a == "--") {
                    args.remove(0);
                }
                Self::Run { env, script, args }
            }
            WorkbenchCli::Env => Self::Env,
        }
    }
}

impl clap::FromArgMatches for WorkbenchCommands {
    fn from_arg_matches(matches: &clap::ArgMatches) -> Result<Self, clap::Error> {
        WorkbenchCli::from_arg_matches(matches).map(Self::from)
    }

    fn from_arg_matches_mut(matches: &mut clap::ArgMatches) -> Result<Self, clap::Error> {
        WorkbenchCli::from_arg_matches_mut(matches).map(Self::from)
    }

    fn update_from_arg_matches(&mut self, matches: &clap::ArgMatches) -> Result<(), clap::Error> {
        *self = Self::from_arg_matches(matches)?;
        Ok(())
    }
}

impl Subcommand for WorkbenchCommands {
    fn augment_subcommands(cmd: clap::Command) -> clap::Command {
        WorkbenchCli::augment_subcommands(cmd)
    }

    fn augment_subcommands_for_update(cmd: clap::Command) -> clap::Command {
        WorkbenchCli::augment_subcommands_for_update(cmd)
    }

    fn has_subcommand(name: &str) -> bool {
        WorkbenchCli::has_subcommand(name)
    }
}

#[derive(Subcommand, Debug)]
enum WorkbenchCli {
    /// Start this checkout's stack (registers it, waits for a slot, builds, bootstraps)
    Up {
        /// Also run these sites (comma-separated)
        #[arg(long, value_delimiter = ',')]
        with: Vec<Site>,
    },
    /// Rebuild and recreate apps (the five Rust apps when none is named)
    Restart {
        /// orchestrator, worker, gateway, auth, rest, studio, docs, landing
        apps: Vec<String>,
    },
    /// Stop a stack (its data stays) and release its slot
    Down {
        /// Another registered stack (default: this checkout's)
        #[arg(long)]
        stack: Option<String>,
    },
    /// Delete a stack: containers, volumes, computes, state and registry entry
    Purge {
        /// Another registered stack (default: this checkout's)
        #[arg(long)]
        stack: Option<String>,
        /// Required to purge main (named as --stack main)
        #[arg(long)]
        yes: bool,
    },
    /// Every registered stack, then the slot queue
    Status,
    /// Compose logs of this checkout's stack
    Logs {
        /// Follow
        #[arg(short, long)]
        follow: bool,
        services: Vec<String>,
    },
    /// psql in metadata-db (default database chelabase_metadata)
    Psql { db: Option<String> },
    /// Run a command in one of the stack's services
    Exec {
        service: String,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        cmd: Vec<String>,
    },
    /// Archive tests on the host and run them in the stack's runner
    Test {
        /// What to archive (cargo nextest archive's package and target selection)
        #[arg(allow_hyphen_values = true, value_terminator = "--")]
        archive: Vec<String>,
        /// After `--`: arguments for nextest run
        #[arg(allow_hyphen_values = true)]
        run: Vec<String>,
    },
    /// Run a repo script (an e2e script) in the stack's runner
    Run {
        /// Extra variables for the runner (KEY=VALUE, repeatable; before the script)
        #[arg(long = "env", value_name = "KEY=VALUE")]
        env: Vec<String>,
        /// The script, then its own arguments: everything after the script's name is the
        /// script's, flags such as --help and --env included
        #[arg(
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true,
            value_name = "SCRIPT [ARGS]"
        )]
        argv: Vec<String>,
    },
    /// Print the path of this checkout's stack.env
    Env,
}

#[derive(Subcommand, Debug)]
enum GenCommands {
    /// Write the TypeScript `Database` and runtime `types` for `@chelabase/client`
    Types {
        /// The gateway's base URL (the data API is under `/data/v1`)
        #[arg(long)]
        url: String,
        /// A secret data key (`chk_sec_…`)
        #[arg(long, env = "CHELABASE_DATA_KEY", hide_env_values = true)]
        key: Option<String>,
        /// Write to this file instead of stdout
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Generate this database (not the main one) as `Database`
        #[arg(long, conflicts_with = "all_databases")]
        database: Option<String>,
        /// Generate every database the key reaches: `Databases` and a `types` map keyed by name
        #[arg(long)]
        all_databases: bool,
    },
}

#[derive(Subcommand, Debug)]
enum DevCommands {
    /// Idempotent ensure: project, API app, demo machine key, tenant org bind
    Bootstrap {
        /// Email of the Zitadel user to make the demo tenant's first admin (default: the dev user)
        #[arg(long)]
        admin_email: Option<String>,
    },
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
                .unwrap_or_else(|_| "chelabase=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Dev { command } => match command {
            DevCommands::Bootstrap { admin_email } => {
                dev::bootstrap::run(admin_email.as_deref()).await
            }
            DevCommands::Token => dev::token::run().await,
        },
        Commands::Gen { command } => match command {
            GenCommands::Types {
                url,
                key,
                out,
                database,
                all_databases,
            } => {
                let which = match (database, all_databases) {
                    (_, true) => generate::Which::All,
                    (Some(d), false) => generate::Which::One(d),
                    (None, false) => generate::Which::Main,
                };
                generate::run(&url, key, out.as_deref(), which).await
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
        Commands::Workbench { command } => {
            use workbench::commands as wb;
            match command {
                WorkbenchCommands::Up { with } => wb::up(&with),
                WorkbenchCommands::Restart { apps } => wb::restart(&apps),
                WorkbenchCommands::Down { stack } => wb::down(stack.as_deref()),
                WorkbenchCommands::Purge { stack, yes } => wb::purge(stack.as_deref(), yes),
                WorkbenchCommands::Status => wb::status(),
                WorkbenchCommands::Logs { follow, services } => wb::logs(follow, &services),
                WorkbenchCommands::Psql { db } => wb::psql(db.as_deref()),
                WorkbenchCommands::Exec { service, cmd } => wb::exec(&service, &cmd),
                WorkbenchCommands::Test { archive, run } => wb::test(&archive, &run),
                WorkbenchCommands::Run { env, script, args } => {
                    wb::run_script(&env, &script, &args)
                }
                WorkbenchCommands::Env => wb::env(),
            }
        }
    }
}

#[cfg(test)]
mod workbench_cli_tests {
    use super::*;

    fn test_args(args: &[&str]) -> (Vec<String>, Vec<String>) {
        let mut argv = vec!["chelabase", "workbench", "test"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).unwrap().command {
            Commands::Workbench {
                command: WorkbenchCommands::Test { archive, run },
            } => (archive, run),
            other => panic!("not workbench test: {other:?}"),
        }
    }

    #[test]
    fn test_with_only_run_args_or_only_a_selection() {
        let (archive, run) = test_args(&["--", "--run-ignored", "only"]);
        assert!(archive.is_empty(), "{archive:?}");
        assert_eq!(run, ["--run-ignored", "only"]);
        let (archive, run) = test_args(&["-p", "rest"]);
        assert_eq!(archive, ["-p", "rest"]);
        assert!(run.is_empty());
    }

    #[test]
    fn exec_needs_a_command_and_run_passes_flags_after_a_double_dash() {
        assert!(Cli::try_parse_from(["chelabase", "workbench", "exec", "worker"]).is_err());
        // The workbench's own flags (`--env`, `--help`) are parsed anywhere; `--` hands
        // everything after it to the script.
        let cli = Cli::try_parse_from(["chelabase", "workbench", "run", "s.sh", "--", "--help"])
            .unwrap();
        let Commands::Workbench {
            command: WorkbenchCommands::Run { env, script, args },
        } = cli.command
        else {
            panic!("not workbench run");
        };
        assert!(env.is_empty());
        assert_eq!(script, "s.sh");
        assert_eq!(args, ["--help"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_accepts_admin_email() {
        let cli = Cli::try_parse_from(["chelabase", "dev", "bootstrap", "--admin-email", "a@b.co"])
            .unwrap();
        let Commands::Dev {
            command: DevCommands::Bootstrap { admin_email },
        } = cli.command
        else {
            panic!("not dev bootstrap");
        };
        assert_eq!(admin_email.as_deref(), Some("a@b.co"));
        let cli = Cli::try_parse_from(["chelabase", "dev", "bootstrap"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Dev {
                command: DevCommands::Bootstrap { admin_email: None }
            }
        ));
    }

    use crate::workbench::commands::Site;

    fn workbench(args: &[&str]) -> WorkbenchCommands {
        let mut argv = vec!["chelabase", "workbench"];
        argv.extend_from_slice(args);
        match Cli::try_parse_from(argv).unwrap().command {
            Commands::Workbench { command } => command,
            other => panic!("not a workbench command: {other:?}"),
        }
    }

    #[test]
    fn workbench_up_takes_sites() {
        let WorkbenchCommands::Up { with } = workbench(&["up", "--with", "docs,landing"]) else {
            panic!()
        };
        assert_eq!(with, vec![Site::Docs, Site::Landing]);
        let WorkbenchCommands::Up { with } = workbench(&["up"]) else { panic!() };
        assert!(with.is_empty());
        assert!(Cli::try_parse_from(["chelabase", "workbench", "up", "--with", "blog"]).is_err());
    }

    #[test]
    fn workbench_test_splits_selection_and_run_args() {
        let WorkbenchCommands::Test { archive, run } = workbench(&[
            "test", "-p", "rest", "--test", "rest_live", "--", "--run-ignored", "only",
        ]) else {
            panic!()
        };
        assert_eq!(archive, ["-p", "rest", "--test", "rest_live"]);
        assert_eq!(run, ["--run-ignored", "only"]);
        let WorkbenchCommands::Test { archive, run } = workbench(&["test"]) else { panic!() };
        assert!(archive.is_empty() && run.is_empty());
    }

    #[test]
    fn workbench_run_takes_env_then_the_script_and_its_args() {
        let WorkbenchCommands::Run { env, script, args } = workbench(&[
            "run", "--env", "A=1", "--env", "B=2", "scripts/neon-e2e.sh", "--fast", "x",
        ]) else {
            panic!()
        };
        assert_eq!(env, ["A=1", "B=2"]);
        assert_eq!(script, "scripts/neon-e2e.sh");
        assert_eq!(args, ["--fast", "x"]);
    }

    #[test]
    fn workbench_exec_passes_the_command_through() {
        let WorkbenchCommands::Exec { service, cmd } =
            workbench(&["exec", "metadata-db", "psql", "-U", "chelabase"])
        else {
            panic!()
        };
        assert_eq!(service, "metadata-db");
        assert_eq!(cmd, ["psql", "-U", "chelabase"]);
    }

    #[test]
    fn workbench_stack_commands() {
        let WorkbenchCommands::Purge { stack, yes } =
            workbench(&["purge", "--stack", "feat-x", "--yes"])
        else {
            panic!()
        };
        assert_eq!(stack.as_deref(), Some("feat-x"));
        assert!(yes);
        assert!(matches!(workbench(&["down"]), WorkbenchCommands::Down { stack: None }));
        let WorkbenchCommands::Restart { apps } = workbench(&["restart", "worker", "gateway"]) else {
            panic!()
        };
        assert_eq!(apps, ["worker", "gateway"]);
        let WorkbenchCommands::Logs { follow, services } = workbench(&["logs", "-f", "worker"]) else {
            panic!()
        };
        assert!(follow);
        assert_eq!(services, ["worker"]);
        assert!(matches!(workbench(&["status"]), WorkbenchCommands::Status));
        assert!(matches!(workbench(&["env"]), WorkbenchCommands::Env));
        let WorkbenchCommands::Psql { db } = workbench(&["psql"]) else { panic!() };
        assert_eq!(db, None);
    }

    // T5 review M5: a script's own flags after its name reach the script.
    #[test]
    fn workbench_run_passes_the_scripts_flags_through() {
        let WorkbenchCommands::Run { env, script, args } =
            workbench(&["run", "scripts/s.sh", "--help", "--env", "x"])
        else {
            panic!()
        };
        assert!(env.is_empty());
        assert_eq!(script, "scripts/s.sh");
        assert_eq!(args, ["--help", "--env", "x"]);
    }

    // T5 review M5: `dev bootstrap` outside a stack's env would recreate the retired zitadel
    // dir under `.chelabase`; it refuses and names the way in.
    #[test]
    fn bootstrap_needs_the_stack_env() {
        let err = crate::dev::bootstrap::check_stack_env(None).unwrap_err().to_string();
        assert!(err.contains("just workbench up"), "{err}");
        assert!(crate::dev::bootstrap::check_stack_env(Some("  ")).is_err());
        assert!(crate::dev::bootstrap::check_stack_env(Some("/m/.chelabase/stacks/main")).is_ok());
    }
}
