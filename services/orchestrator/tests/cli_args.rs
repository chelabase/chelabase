//! The `orchestrator` binary's command line: --help, --version, an unknown argument; no arguments starts
//! the service (covered by its other tests).
use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_orchestrator"))
        .args(args)
        // A clean environment: the arguments must be handled before any config is read.
        .env_clear()
        .output()
        .expect("spawn orchestrator")
}

#[test]
fn cli_help_exits_zero_and_names_the_binary() {
    for flag in ["--help", "-h"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{flag}: {:?}", out.status);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("orchestrator") && text.contains("CHELABASE_"), "{flag}: {text}");
    }
}

#[test]
fn cli_version_prints_name_and_version() {
    for flag in ["--version", "-V"] {
        let out = run(&[flag]);
        assert!(out.status.success(), "{flag}: {:?}", out.status);
        let want = format!("orchestrator {}\n", env!("CARGO_PKG_VERSION"));
        assert_eq!(String::from_utf8_lossy(&out.stdout), want);
    }
}

#[test]
fn cli_unknown_argument_exits_two() {
    let out = run(&["--bogus"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--bogus") && err.contains("--help"), "{err}");
}
