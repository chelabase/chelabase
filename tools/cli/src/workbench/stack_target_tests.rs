//! workbench-stack T1: `test`/`run` on a named stack (spec Parts 1 and 2).

use super::commands::{Stack, runner_command};
use super::registry::{Entry, Registry};
use super::target::{
    STACK_FILE, Target, archive_path, base_stack_slug, check_ready, read_recorded, resolve_target,
};
use std::path::{Path, PathBuf};
use std::process::Command;

fn entry(slug: &str, running: bool) -> Entry {
    Entry {
        slug: slug.into(),
        checkout: PathBuf::from(format!("/m/.worktrees/{slug}")),
        gateway_port: 8101,
        proxy_port: 4501,
        created_at: 1,
        running_since: running.then_some(2),
    }
}

fn other(s: &str) -> Target {
    Target::Other(s.to_string())
}

#[test]
fn the_flag_wins_then_the_recorded_file_then_the_own_stack() {
    assert_eq!(resolve_target(Some("feat"), Some("other"), "feat-t1").unwrap(), other("feat"));
    assert_eq!(resolve_target(Some(" feat "), None, "feat-t1").unwrap(), other("feat"));
    assert_eq!(
        resolve_target(Some("feat-t1"), Some("feat"), "feat-t1").unwrap(),
        Target::Own,
        "naming one's own stack is the own stack"
    );
    assert_eq!(resolve_target(None, Some("feat"), "feat-t1").unwrap(), other("feat"));
    assert_eq!(resolve_target(None, Some("feat\n"), "feat-t1").unwrap(), other("feat"));
    assert_eq!(resolve_target(None, None, "feat-t1").unwrap(), Target::Own);
    assert_eq!(
        resolve_target(Some("feat"), Some(""), "feat-t1").unwrap(),
        other("feat"),
        "--stack overrides an empty record"
    );
}

#[test]
fn an_empty_record_or_flag_refuses_with_a_hint() {
    let err = resolve_target(None, Some(""), "feat-t1").unwrap_err().to_string();
    assert!(err.contains("--stack") && err.contains("just workbench up"), "{err}");
    let err = resolve_target(None, Some("  \n"), "feat-t1").unwrap_err().to_string();
    assert!(err.contains("--stack"), "{err}");
    assert!(resolve_target(Some(""), None, "feat-t1").is_err());
    assert!(resolve_target(Some("  "), Some("feat"), "feat-t1").is_err());
}

#[test]
fn the_record_is_read_from_the_checkout() {
    assert_eq!(STACK_FILE, ".workbench-stack");
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(read_recorded(tmp.path()).unwrap(), None);
    std::fs::write(tmp.path().join(STACK_FILE), "feat\n").unwrap();
    assert_eq!(read_recorded(tmp.path()).unwrap().as_deref().map(str::trim), Some("feat"));
    std::fs::write(tmp.path().join(STACK_FILE), "").unwrap();
    assert_eq!(read_recorded(tmp.path()).unwrap().as_deref(), Some(""));
}

#[test]
fn another_stack_must_be_registered_running_and_fully_up() {
    let apps = ["orchestrator", "studio"];
    let up = vec!["orchestrator".to_string(), "studio".to_string(), "proxy".to_string()];
    let err = check_ready("feat", None, &apps, &up).unwrap_err().to_string();
    assert!(err.contains("no stack \"feat\" is registered"), "{err}");
    let err = check_ready("feat", Some(&entry("feat", false)), &apps, &up).unwrap_err().to_string();
    assert!(
        err.contains("isn't running") && err.contains("just workbench up") && err.contains("/m/.worktrees/feat"),
        "{err}"
    );
    let err = check_ready("feat", Some(&entry("feat", true)), &apps, &up[..1]).unwrap_err().to_string();
    assert!(err.contains("studio") && err.contains("just workbench up"), "{err}");
    check_ready("feat", Some(&entry("feat", true)), &apps, &up).unwrap();
}

#[test]
fn each_checkout_archives_to_its_own_file() {
    let state = Path::new("/m/.chelabase/stacks/feat");
    assert_eq!(archive_path(state, "feat", "feat"), state.join("test/archive.tar.zst"));
    assert_eq!(archive_path(state, "feat", "feat-t1"), state.join("test/feat-t1.tar.zst"));
    assert_ne!(archive_path(state, "feat", "feat-t1"), archive_path(state, "feat", "feat-t2"));
}

#[test]
fn a_task_worktree_records_its_bases_registered_stack() {
    let mut reg = Registry::default();
    reg.stacks.push(entry("feat-x", false));
    assert_eq!(base_stack_slug("feat/x", &reg).unwrap(), "feat-x", "registered, running or not");
    assert_eq!(base_stack_slug("feat-x", &reg).unwrap(), "feat-x");
    assert_eq!(base_stack_slug("other", &reg).unwrap(), "", "no stack: empty");
    assert_eq!(base_stack_slug("main", &reg).unwrap(), "", "main unregistered: empty");
    reg.stacks.push(entry("main", true));
    assert_eq!(base_stack_slug("main", &reg).unwrap(), "main", "main's own stack, not main-wt");
}

fn stack_with_env(tmp: &Path) -> Stack {
    let main = tmp.join("m");
    let stack = Stack::new("feat", &main.join(".worktrees/feat"), &main);
    std::fs::create_dir_all(stack.env_file.parent().unwrap()).unwrap();
    std::fs::write(
        &stack.env_file,
        format!("CHELABASE_CHECKOUT={}\nCHELABASE_PUBLIC_SCHEME=http\n", stack.checkout.display()),
    )
    .unwrap();
    stack
}

fn env_of(cmd: &Command, key: &str) -> Option<String> {
    cmd.get_envs()
        .find(|(k, _)| *k == key)
        .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
}

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
}

fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

#[test]
fn another_checkouts_run_mounts_that_checkout_in_the_stacks_runner() {
    let tmp = tempfile::tempdir().unwrap();
    let stack = stack_with_env(tmp.path());
    let task = tmp.path().join("m/.worktrees/feat-t1");
    let cmd = runner_command(
        &stack,
        &task,
        &[("A".to_string(), "1".to_string())],
        &["scripts/x.sh".to_string(), "--y".to_string()],
    )
    .unwrap();
    assert_eq!(
        env_of(&cmd, "CHELABASE_CHECKOUT").as_deref(),
        Some(task.to_str().unwrap()),
        "Compose interpolates the mount and working_dir from the process env"
    );
    let args = args_of(&cmd);
    assert!(has_pair(&args, "-e", &format!("CHELABASE_CHECKOUT={}", task.display())), "{args:?}");
    assert!(has_pair(&args, "-e", "A=1"), "{args:?}");
    assert!(has_pair(&args, "-p", "chelabase-feat"), "{args:?}");
    assert!(has_pair(&args, "--env-file", stack.env_file.to_str().unwrap()), "{args:?}");
    assert!(has_pair(&args, "--profile", "runner"), "{args:?}");
    let runner = args.iter().rposition(|a| a == "runner").unwrap();
    assert_eq!(&args[runner + 1..], ["scripts/x.sh", "--y"]);
    for flag in ["run", "--rm", "--no-deps"] {
        assert!(args[..runner].iter().any(|a| a == flag), "{flag}: {args:?}");
    }
}

#[test]
fn the_stacks_own_checkout_runs_as_today() {
    let tmp = tempfile::tempdir().unwrap();
    let stack = stack_with_env(tmp.path());
    let cmd = runner_command(&stack, &stack.checkout.clone(), &[], &["s.sh".to_string()]).unwrap();
    assert_eq!(
        env_of(&cmd, "CHELABASE_CHECKOUT").as_deref(),
        Some(stack.checkout.to_str().unwrap()),
        "the env file's own value"
    );
    assert!(!args_of(&cmd).iter().any(|a| a.starts_with("CHELABASE_CHECKOUT=")), "no override");
}
