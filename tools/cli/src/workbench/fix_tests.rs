//! T3 review fix round (W2): one failing test per finding, written before the fix.

use super::env::example_scheme;
use super::registry::{Entry, Registry, with_registry};
use super::slots::{
    SlotOptions, acquire_slot, drop_tickets, own_ticket_content, queued, ticket_name,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn entry(slug: &str, gw: u16, pg: u16, running: Option<u64>) -> Entry {
    Entry {
        slug: slug.to_string(),
        checkout: PathBuf::from(format!("/w/{slug}")),
        gateway_port: gw,
        proxy_port: pg,
        created_at: 1,
        running_since: running,
    }
}

fn opts(cap: usize, timeout_ms: u64) -> SlotOptions {
    SlotOptions {
        cap,
        timeout: Duration::from_millis(timeout_ms),
        poll: Duration::from_millis(20),
        status_every: Duration::from_millis(50),
    }
}

fn seed(dir: &Path, entries: Vec<Entry>) {
    with_registry(dir, |r| {
        r.stacks = entries;
        Ok(())
    })
    .unwrap();
}

fn running_slugs(dir: &Path) -> Vec<String> {
    with_registry(dir, |r| Ok(r.running().map(|e| e.slug.clone()).collect())).unwrap()
}

fn plant(dir: &Path, nanos: u128, slug: &str, content: &str) -> PathBuf {
    let queue = dir.join("queue");
    std::fs::create_dir_all(&queue).unwrap();
    let path = queue.join(ticket_name(nanos, slug));
    std::fs::write(&path, content).unwrap();
    path
}

// I1: a waiter whose stack is started by another process leaves no ticket behind.
#[test]
fn a_waiter_whose_stack_started_elsewhere_leaves_no_ticket() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("a", 8100, 4500, Some(0)), entry("b", 8101, 4501, None)]);
    let d = dir.path().to_path_buf();
    let starter = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        with_registry(&d, |r| {
            r.get_mut("b").unwrap().running_since = Some(5);
            Ok(())
        })
        .unwrap();
    });
    acquire_slot(dir.path(), "b", &opts(1, 3000), &|_| true, &|| 9, &mut |_| {}).unwrap();
    starter.join().unwrap();
    assert!(queued(dir.path()).unwrap().is_empty(), "no ticket left behind");
}

// I1: an error during the wait (the stack was purged) leaves no ticket behind either.
#[test]
fn a_waiter_whose_stack_was_purged_leaves_no_ticket() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("a", 8100, 4500, Some(0)), entry("b", 8101, 4501, None)]);
    let d = dir.path().to_path_buf();
    let purger = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        with_registry(&d, |r| {
            r.remove("b");
            Ok(())
        })
        .unwrap();
    });
    assert!(acquire_slot(dir.path(), "b", &opts(1, 3000), &|_| true, &|| 9, &mut |_| {}).is_err());
    purger.join().unwrap();
    assert!(queued(dir.path()).unwrap().is_empty(), "no ticket left behind");
}

// I3: two checkouts whose names map to one slug never share a stack.
#[test]
fn two_checkouts_cannot_share_a_slug() {
    let tmp = tempfile::tempdir().unwrap();
    let first = tmp.path().join("feat_x");
    let second = tmp.path().join("feat-x");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    let mut r = Registry::default();
    r.ensure("feat-x", &first, 1, |_| true).unwrap();
    let err = r.ensure("feat-x", &second, 2, |_| true).unwrap_err().to_string();
    assert!(err.contains("feat_x") && err.contains("feat-x"), "{err}");
    assert_eq!(r.get("feat-x").unwrap().checkout, first, "unchanged");
}

// I3: a checkout that no longer exists hands its stack to the new one (same ports, same data).
#[test]
fn a_moved_checkout_takes_its_stack_over() {
    let tmp = tempfile::tempdir().unwrap();
    let gone = tmp.path().join("old-place");
    let here = tmp.path().join("new-place");
    std::fs::create_dir_all(&here).unwrap();
    let mut r = Registry::default();
    let before = r.ensure("x", &gone, 1, |_| true).unwrap();
    let after = r.ensure("x", &here, 2, |_| true).unwrap();
    assert_eq!(after.checkout, here);
    assert_eq!((after.gateway_port, after.proxy_port), (before.gateway_port, before.proxy_port));
    assert_eq!(r.get("x").unwrap().checkout, here, "recorded");
}

// M4: a ticket names its process by pid and start time, so a reused pid doesn't keep it alive.
#[test]
fn our_own_ticket_content_names_this_process() {
    let content = own_ticket_content();
    let (pid, start) = content.split_once(' ').expect("`<pid> <starttime>`");
    assert_eq!(pid, std::process::id().to_string());
    assert!(start.parse::<u64>().is_ok(), "{content}");
}

#[test]
fn a_ticket_whose_pid_was_reused_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("b", 8101, 4501, None), entry("c", 8102, 4502, None)]);
    // this process's pid, but a start time it never had
    let stale = plant(dir.path(), 1, "b", &format!("{} 1", std::process::id()));
    acquire_slot(dir.path(), "c", &opts(1, 1000), &|_| true, &|| 9, &mut |_| {}).unwrap();
    assert_eq!(running_slugs(dir.path()), ["c"]);
    assert!(!stale.exists());
}

#[test]
fn a_ticket_of_a_live_process_still_holds_its_place() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("b", 8101, 4501, None), entry("c", 8102, 4502, None)]);
    let live = plant(dir.path(), 1, "b", &own_ticket_content());
    assert!(acquire_slot(dir.path(), "c", &opts(1, 150), &|_| true, &|| 9, &mut |_| {}).is_err());
    assert!(running_slugs(dir.path()).is_empty());
    assert!(live.exists());
}

// M5: a clock that steps back can't put a newcomer ahead of an older waiter.
#[test]
fn a_clock_step_back_cannot_jump_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), vec![entry("b", 8101, 4501, None), entry("c", 8102, 4502, None)]);
    // b queued while the clock was far ahead
    plant(dir.path(), 10u128.pow(38), "b", &own_ticket_content());
    let err = acquire_slot(dir.path(), "c", &opts(1, 150), &|_| true, &|| 9, &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(err.contains('b'), "{err}");
    assert!(running_slugs(dir.path()).is_empty(), "c stayed behind b");
}

// M6: a scheme that is neither http nor https is refused, as entry.sh refuses it.
#[test]
fn the_example_scheme_is_checked() {
    assert_eq!(example_scheme("CHELABASE_PUBLIC_SCHEME=http\n").unwrap(), "http");
    assert_eq!(example_scheme("CHELABASE_PUBLIC_SCHEME= HTTPS \n").unwrap(), "https");
    assert_eq!(example_scheme("CHELABASE_PUBLIC_SCHEME=\n").unwrap(), "https");
    assert_eq!(example_scheme("# no scheme here\n").unwrap(), "https");
    let err = example_scheme("CHELABASE_PUBLIC_SCHEME=htp\n").unwrap_err().to_string();
    assert!(err.contains("htp") && err.contains("CHELABASE_PUBLIC_SCHEME"), "{err}");
}

// M7: `status` lists the queue and `purge` drops a stack's tickets.
#[test]
fn the_queue_can_be_listed_and_a_stacks_tickets_dropped() {
    let dir = tempfile::tempdir().unwrap();
    assert!(queued(dir.path()).unwrap().is_empty(), "no queue dir is an empty queue");
    for (n, slug) in [(3u128, "b"), (1, "b"), (2, "feat-x")] {
        plant(dir.path(), n, slug, &own_ticket_content());
    }
    assert_eq!(
        queued(dir.path()).unwrap(),
        [
            (ticket_name(1, "b"), "b".to_string()),
            (ticket_name(2, "feat-x"), "feat-x".to_string()),
            (ticket_name(3, "b"), "b".to_string()),
        ]
    );
    assert_eq!(drop_tickets(dir.path(), "b").unwrap(), 2);
    assert_eq!(
        queued(dir.path()).unwrap(),
        [(ticket_name(2, "feat-x"), "feat-x".to_string())]
    );
}
