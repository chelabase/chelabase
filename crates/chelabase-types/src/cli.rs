//! The command line every service binary shares: `--help`, `--version`, and nothing else.
//! Called first in `main`, before any environment or config is read.

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Cli {
    /// No arguments: start the service.
    Run,
    /// Print this text to stdout and exit 0.
    Print(String),
    /// Print this line to stderr and exit 2.
    Reject(String),
}

/// Decide what `args` (without the program name) ask for.
pub fn parse(name: &str, about: &str, version: &str, args: &[String]) -> Cli {
    match args.first().map(String::as_str) {
        None => Cli::Run,
        Some("--help" | "-h") => Cli::Print(format!(
            "{name} {version}\n{about}\n\nUsage: {name} [--help | --version]\n\n\
             It takes no other arguments: it is configured through CHELABASE_* environment\n\
             variables. See the self-hosting docs at https://github.com/chelabase/chelabase.\n"
        )),
        Some("--version" | "-V") => Cli::Print(format!("{name} {version}\n")),
        Some(other) => Cli::Reject(format!(
            "{name}: unexpected argument '{other}'; try '{name} --help'"
        )),
    }
}

/// Handle the process's arguments: return to start the service, or print and exit.
pub fn handle(name: &str, about: &str, version: &str) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(name, about, version, &args) {
        Cli::Run => {}
        Cli::Print(text) => {
            print!("{text}");
            std::process::exit(0);
        }
        Cli::Reject(line) => {
            eprintln!("{line}");
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Cli {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        parse("svc", "A service.", "1.2.3", &args)
    }

    #[test]
    fn cli_parse_covers_help_version_unknown_and_none() {
        assert_eq!(p(&[]), Cli::Run);
        for f in ["--help", "-h"] {
            let Cli::Print(t) = p(&[f]) else { panic!("{f}") };
            assert!(t.contains("svc") && t.contains("A service.") && t.contains("CHELABASE_"));
        }
        for f in ["--version", "-V"] {
            assert_eq!(p(&[f]), Cli::Print("svc 1.2.3\n".into()));
        }
        let Cli::Reject(l) = p(&["--bogus"]) else { panic!() };
        assert!(l.contains("--bogus") && l.contains("--help") && !l.contains('\n'));
    }
}
