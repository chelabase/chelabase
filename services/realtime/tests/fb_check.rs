//! RT1 final fix round (batch B, T8): a window's check and the conversion of its keys to the
//! session's form run in one transaction. `CheckRequest::convert` carries the keys to convert
//! (every key of the window: inserts, updates, deletes, in that order) and
//! `CheckAnswer::converted` answers them in that order, from the same session as the check's
//! rows, so a setting changed in between can't make the two disagree. `Checker::convert` (a
//! transaction of its own) is no longer used for windows.

mod fakes;

use std::time::Duration;

use fakes::*;
use realtime::check::{CONCURRENCY, Limiter, VisibilityCache};
use realtime::filter::{FilterSettings, TableFilter};
use realtime::merge::RelWindow;
use realtime::protocol::{Change, ChangeOp};
use realtime::source::ChannelEvent;
use tokio::sync::mpsc;
use tokio::time::Instant;

fn k(t: &str) -> Vec<String> {
    vec![t.to_string()]
}

fn window(inserts: &[&str], updates: &[&str], deletes: &[&str]) -> RelWindow {
    RelWindow {
        rel: REL,
        changed: false,
        inserts: inserts.iter().map(|t| k(t)).collect(),
        updates: updates.iter().map(|t| k(t)).collect(),
        deletes: deletes.iter().map(|t| k(t)).collect(),
    }
}

#[tokio::test(start_paused = true)]
async fn a_windows_check_converts_its_keys_in_the_same_session() {
    let checker = FakeChecker::new();
    checker.insert_rows(REL, &["1", "2"]);
    let mut f = TableFilter::new(
        target(),
        relation(REL, "items"),
        checker.clone(),
        Limiter::new(CONCURRENCY),
        VisibilityCache::new(Duration::from_millis(100)),
        FilterSettings::default(),
    );
    let (tx, mut bypass) = mpsc::channel(16);
    f.add(1, role("billing"), true, tx);
    let (tx, mut owner_rx) = mpsc::channel(16);
    f.add(2, owner("member-token"), true, tx);
    // The xid becomes visible on the second poll: only the attempt that answers counts.
    checker.invisible_for(10, 1);
    f.deliver(&window(&["1"], &["2"], &["3"]), &[10], Instant::now())
        .await;
    assert_eq!(
        checker.separate_converts(),
        0,
        "no conversion in a transaction of its own: {:?}",
        checker.calls()
    );
    let every_key = vec![k("1"), k("2"), k("3")];
    let converts = checker.check_converts();
    assert!(
        converts.iter().filter(|c| **c == every_key).count() >= 2,
        "each group's answering check carried every key of the window: {converts:?}"
    );
    let expected = vec![ChannelEvent::Rows(vec![
        Change {
            op: ChangeOp::Insert,
            keys: Some(vec![ks("1")]),
        },
        Change {
            op: ChangeOp::Update,
            keys: Some(vec![ks("2")]),
        },
        Change {
            op: ChangeOp::Delete,
            keys: Some(vec![ks("3")]),
        },
    ])];
    assert_eq!(drain(&mut bypass), expected);
    assert_eq!(drain(&mut owner_rx), expected);
}

#[tokio::test(start_paused = true)]
async fn a_failed_probe_converts_nothing_and_is_changed() {
    // No trustworthy session form without a passing probe: `changed`, never keys.
    let checker = FakeChecker::new();
    checker.insert_rows(REL, &["1"]);
    checker.fail_probe(Some("billing"), true);
    let mut f = TableFilter::new(
        target(),
        relation(REL, "items"),
        checker.clone(),
        Limiter::new(CONCURRENCY),
        VisibilityCache::new(Duration::from_millis(100)),
        FilterSettings::default(),
    );
    let (tx, mut rx) = mpsc::channel(16);
    f.add(1, role("billing"), true, tx);
    f.deliver(&window(&["1"], &[], &[]), &[10], Instant::now())
        .await;
    assert_eq!(checker.separate_converts(), 0);
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![Change {
            op: ChangeOp::Changed,
            keys: None,
        }])],
        "a failed probe"
    );
}
