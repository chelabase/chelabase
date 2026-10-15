//! workbench-stack end-of-step fix round: failing tests first (I1, m1, m2, M3).

use super::commands::{Stack, runner_command};
use super::registry::{Entry, Registry};
use super::target::{base_stack_slug, check_up_allowed, resolve_target};
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn args_of(cmd: &Command) -> Vec<String> {
    cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
}

fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2).any(|w| w[0] == flag && w[1] == value)
}

// I1: a suite that recreates the stack's apps through Compose (roles_live) must interpolate the
// stack's own checkout, never the task checkout the runner works in.
#[test]
fn the_runner_always_names_the_stacks_own_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let stack = stack_with_env(tmp.path());
    let own = format!("CHELABASE_STACK_CHECKOUT={}", stack.checkout.display());
    let task = tmp.path().join("m/.worktrees/feat-t1");
    let other = args_of(&runner_command(&stack, &task, &[], &["s.sh".to_string()]).unwrap());
    assert!(has_pair(&other, "-e", &own), "{other:?}");
    let same = args_of(&runner_command(&stack, &stack.checkout.clone(), &[], &["s.sh".to_string()]).unwrap());
    assert!(has_pair(&same, "-e", &own), "{same:?}");
}

// m1: `up` in a worktree that records another stack would start the second stack the record
// exists to prevent.
#[test]
fn up_refuses_where_another_stack_is_recorded() {
    check_up_allowed(None, "feat-t1").unwrap();
    check_up_allowed(Some(""), "feat-t1").unwrap();
    check_up_allowed(Some("\n"), "feat-t1").unwrap();
    check_up_allowed(Some("feat-t1\n"), "feat-t1").unwrap();
    let err = check_up_allowed(Some("feat\n"), "feat-t1").unwrap_err().to_string();
    assert!(err.contains("feat") && err.contains(".workbench-stack"), "{err}");
}

// m2: a remote-tracking base names the same stack as its local branch.
#[test]
fn a_remote_base_names_its_branchs_stack() {
    let mut reg = Registry::default();
    reg.stacks.push(Entry {
        slug: "feat-x".into(),
        checkout: PathBuf::from("/m/.worktrees/feat-x"),
        gateway_port: 8101,
        proxy_port: 4501,
        created_at: 1,
        running_since: None,
        control_slot: None,
    });
    assert_eq!(base_stack_slug("origin/feat/x", &reg).unwrap(), "feat-x");
    assert_eq!(base_stack_slug("origin/feat-x", &reg).unwrap(), "feat-x");
}

// M3 (T1 review): the refusal says how to get back to the worktree's own stack.
#[test]
fn an_empty_record_says_deleting_it_uses_the_own_stack() {
    let err = resolve_target(None, Some(""), "feat-t1").unwrap_err().to_string();
    assert!(err.contains("delete") && err.contains(".workbench-stack"), "{err}");
}

// Live pass L1: the runner mounts only the task checkout, so the stack env's CHELABASE_CLI (a path
// in the stack's own checkout) doesn't exist there; e2e scripts use the task checkout's CLI.
#[test]
fn another_checkouts_run_uses_that_checkouts_cli() {
    let tmp = tempfile::tempdir().unwrap();
    let stack = stack_with_env(tmp.path());
    let task = tmp.path().join("m/.worktrees/feat-t1");
    let other = args_of(&runner_command(&stack, &task, &[], &["s.sh".to_string()]).unwrap());
    let cli = format!("CHELABASE_CLI={}", task.join("target/debug/chelabase").display());
    assert!(has_pair(&other, "-e", &cli), "{other:?}");
    let same = args_of(&runner_command(&stack, &stack.checkout.clone(), &[], &["s.sh".to_string()]).unwrap());
    assert!(!same.iter().any(|a| a.starts_with("CHELABASE_CLI=")), "the stack env's own value: {same:?}");
}
