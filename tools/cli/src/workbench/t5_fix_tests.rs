//! T5 review fix round (W2): one failing test per finding, written before the fix.

use super::commands::{
    alive_ps_args, check_purge, compose_command, is_legacy_compute, missing_services,
    nextest_bin_for_env, stop_processes, up_recreates,
};
use super::registry::{Entry, with_registry};
use super::slots::touch_slot;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn labels(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

// I1: only a W1 compute (named chelabase-compute-*, endpoint label, no stack label) is legacy;
// a worktree stack whose slug starts with "compute" is never touched.
#[test]
fn only_w1_computes_are_legacy() {
    let ep = [("chelabase.dev/endpoint-id", "ep-abc123")];
    assert!(is_legacy_compute("chelabase-compute-ep-abc123", &labels(&ep)));
    assert!(is_legacy_compute("/chelabase-compute-ep-abc123", &labels(&ep)), "docker's leading /");
    let worktree_service = [("com.docker.compose.project", "chelabase-compute-autosuspend")];
    assert!(!is_legacy_compute("chelabase-compute-autosuspend-worker-1", &labels(&worktree_service)));
    let scoped = [("chelabase.dev/endpoint-id", "ep-abc123"), ("chelabase.dev/stack", "compute-x")];
    assert!(!is_legacy_compute("chelabase-compute-x-compute-ep-abc123", &labels(&scoped)));
    assert!(!is_legacy_compute("chelabase-main-compute-ep-abc123", &labels(&ep)), "new names");
    assert!(!is_legacy_compute("chelabase-compute-ep-abc123", &labels(&[])), "no endpoint label");
}

// I2: `up` on a running stack recreates every app whose binary changed, plus those whose env
// files changed, before it records the binaries.
#[test]
fn up_recreates_stale_binaries_and_changed_env_users() {
    let recorded = HashMap::from([("worker".to_string(), 1u64), ("gateway".to_string(), 2)]);
    let current = HashMap::from([("worker".to_string(), 9u64), ("gateway".to_string(), 2)]);
    assert_eq!(
        up_recreates(&recorded, &current, &["orchestrator".to_string(), "worker".to_string()]),
        ["orchestrator", "worker"]
    );
    assert!(up_recreates(&current, &current, &[]).is_empty());
}

// I3: legacy processes are waited for; one that ignores SIGTERM is killed after the grace.
#[test]
fn stopping_processes_waits_and_then_kills() {
    let mut polite = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let mut stubborn = std::process::Command::new("sh")
        .args(["-c", "trap '' TERM; exec sleep 30"])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(100)); // let the trap install
    let started = std::time::Instant::now();
    stop_processes(&[polite.id(), stubborn.id()], Duration::from_millis(500)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(polite.wait().unwrap().signal(), Some(15), "SIGTERM first");
    assert_eq!(stubborn.wait().unwrap().signal(), Some(9), "SIGKILL after the grace");
    stop_processes(&[999_999_999], Duration::from_millis(50)).unwrap();
}

// I4: purging main needs `--stack main` named explicitly and --yes; a wrong cwd can't reach it.
#[test]
fn main_is_purged_only_by_name() {
    assert!(check_purge("main", None, true).is_err(), "resolved from the cwd");
    assert!(check_purge("main", Some("main"), false).is_err(), "no --yes");
    assert!(check_purge("main", Some("main"), true).is_ok());
    assert!(check_purge("feat-x", None, false).is_ok());
    assert!(check_purge("feat-x", Some("feat-x"), true).is_ok());
}

// I5: every compose call carries the stack's env, so a shell's exported settings can't win.
#[test]
fn compose_runs_with_the_stack_env() {
    let dir = tempfile::tempdir().unwrap();
    let env = dir.path().join("stack.env");
    std::fs::write(&env, "# c\nCHELABASE_STACK_DIR=/s\nCHELABASE_GATEWAY_PORT=8137\n").unwrap();
    let cmd = compose_command(&env, vec!["compose".into(), "ps".into()]).unwrap();
    let envs: HashMap<String, Option<String>> = cmd
        .get_envs()
        .map(|(k, v)| {
            (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))
        })
        .collect();
    assert_eq!(envs.get("CHELABASE_STACK_DIR"), Some(&Some("/s".to_string())));
    assert_eq!(envs.get("CHELABASE_GATEWAY_PORT"), Some(&Some("8137".to_string())));
    assert_eq!(cmd.get_program(), "docker");
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    assert_eq!(args, ["compose", "ps"]);
}

// M2: without a host cargo-nextest the runner still starts (an empty bind), so `run` works.
#[test]
fn a_missing_nextest_still_lets_the_runner_start() {
    assert_eq!(nextest_bin_for_env(Some(Path::new("/h/cargo-nextest"))), "/h/cargo-nextest");
    assert_eq!(nextest_bin_for_env(None), "/dev/null");
}

// M3: a half-up stack is brought up, not taken as running.
#[test]
fn missing_services_are_found() {
    let running = vec!["gateway".to_string(), "zitadel".to_string(), "worker".to_string()];
    assert_eq!(
        missing_services(&["orchestrator", "worker", "gateway", "studio"], &running),
        ["orchestrator", "studio"]
    );
    assert!(missing_services(&["gateway"], &running).is_empty());
}

// M4: liveness asks Docker by project label (no compose file needed), and `up` refreshes its
// slot after the builds so a long build isn't reaped.
#[test]
fn liveness_needs_no_compose_file() {
    assert_eq!(
        alive_ps_args("chelabase-feat-x"),
        [
            "ps",
            "-q",
            "--filter",
            "label=com.docker.compose.project=chelabase-feat-x",
            "--filter",
            "status=running"
        ]
    );
}

#[test]
fn a_running_slot_is_refreshed() {
    let dir = tempfile::tempdir().unwrap();
    let e = |slug: &str, running: Option<u64>| Entry {
        slug: slug.to_string(),
        checkout: PathBuf::from("/w"),
        gateway_port: 8100,
        proxy_port: 4500,
        created_at: 1,
        running_since: running,
        control_slot: None,
    };
    with_registry(dir.path(), |r| {
        r.stacks = vec![e("a", Some(10)), e("b", None)];
        Ok(())
    })
    .unwrap();
    touch_slot(dir.path(), "a", 99).unwrap();
    touch_slot(dir.path(), "b", 99).unwrap();
    let (a, b) = with_registry(dir.path(), |r| {
        Ok((r.get("a").unwrap().running_since, r.get("b").unwrap().running_since))
    })
    .unwrap();
    assert_eq!(a, Some(99));
    assert_eq!(b, None, "a stopped stack takes no slot by being touched");
}
