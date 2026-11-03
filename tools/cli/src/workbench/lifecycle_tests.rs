//! W2 spec sections 4 and 6: main's one-time move, and which apps restart.

use super::commands::{curlrc, stale_apps};
use super::migrate::migrate_main_layout;
use super::slug::hosts;
use std::collections::HashMap;
use std::path::Path;

fn touch(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn the_old_layout_moves_into_main_stack_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let m = tmp.path();
    touch(&m.join(".chelabase/neon/proxy-ca.crt"), "ca");
    touch(&m.join(".chelabase/neon/compute-jwt.key"), "k");
    touch(&m.join(".chelabase/secrets/secrets.key"), "s");
    touch(&m.join(".chelabase/dev/pids"), "");
    touch(&m.join(".chelabase/dev/orchestrator.log"), "log");
    touch(&m.join(".chelabase/live/lock"), "");
    touch(&m.join("apps/studio/.env.local"), "VITE_API_URL=x");
    let linked = m.join(".worktrees/feat-x/apps/studio");
    std::fs::create_dir_all(&linked).unwrap();
    std::os::unix::fs::symlink(m.join("apps/studio/.env.local"), linked.join(".env.local")).unwrap();
    touch(&m.join(".worktrees/other/apps/studio/.env.local"), "a real file");

    let done = migrate_main_layout(m).unwrap();
    assert!(!done.is_empty(), "it says what it did");

    let s = m.join(".chelabase/stacks/main");
    assert_eq!(read(&s.join("neon/proxy-ca.crt")), "ca");
    assert!(!m.join(".chelabase/neon/proxy-ca.crt").exists());
    assert_eq!(read(&m.join(".chelabase/neon/compute-jwt.key")), "k", "machine keys stay");
    assert_eq!(read(&m.join(".chelabase/secrets/secrets.key")), "s", "machine keys stay");
    assert!(!m.join(".chelabase/dev").exists());
    assert!(!m.join(".chelabase/live").exists());
    assert!(!m.join("apps/studio/.env.local").exists());
    assert!(
        std::fs::symlink_metadata(linked.join(".env.local")).is_err(),
        "the worktree's link to it is gone"
    );
    assert_eq!(
        read(&m.join(".worktrees/other/apps/studio/.env.local")),
        "a real file",
        "a real file is not ours to delete"
    );
}

#[test]
fn a_second_run_does_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let m = tmp.path();
    touch(&m.join(".chelabase/neon/proxy-ca.crt"), "ca");
    migrate_main_layout(m).unwrap();
    assert!(migrate_main_layout(m).unwrap().is_empty());
    assert_eq!(read(&m.join(".chelabase/stacks/main/neon/proxy-ca.crt")), "ca");
}

#[test]
fn a_fresh_checkout_has_nothing_to_move() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(migrate_main_layout(tmp.path()).unwrap().is_empty());
}

#[test]
fn both_layouts_present_is_an_error_and_nothing_moves() {
    let tmp = tempfile::tempdir().unwrap();
    let m = tmp.path();
    touch(&m.join(".chelabase/neon/proxy-ca.crt"), "old");
    touch(&m.join(".chelabase/stacks/main/neon/proxy-ca.crt"), "new");
    let err = migrate_main_layout(m).unwrap_err().to_string();
    assert!(err.contains(".chelabase/neon/proxy-ca.crt"), "{err}");
    assert_eq!(read(&m.join(".chelabase/neon/proxy-ca.crt")), "old");
    assert_eq!(read(&m.join(".chelabase/stacks/main/neon/proxy-ca.crt")), "new");
}

#[test]
fn only_apps_whose_binary_changed_restart() {
    let recorded = HashMap::from([("worker".to_string(), 1u64), ("gateway".to_string(), 2)]);
    let current = HashMap::from([
        ("worker".to_string(), 1u64),
        ("gateway".to_string(), 3),
        ("rest".to_string(), 5),
    ]);
    assert_eq!(stale_apps(&recorded, &current), ["gateway", "rest"]);
    assert!(stale_apps(&current, &current).is_empty());
}

#[test]
fn the_runners_curl_reaches_the_gateway_for_every_stack_host() {
    let h = hosts("feat-x", 8137);
    let rc = curlrc(&h, 8137);
    for name in h.names() {
        assert!(
            rc.lines().any(|l| l == format!("connect-to = \"{name}:8137:gateway:8137\"")),
            "{name}: {rc}"
        );
    }
    assert_eq!(rc.lines().filter(|l| l.starts_with("connect-to")).count(), 6);
}
