//! RT1 T8: per-role filtering (spec §4; R2, R12, R15, R19, W2, W6/P7, W26, W27, S11, S30) on
//! [`TableFilter`] with the fake checker, plus the R12 cache, the R15 concurrency bound, the
//! owner's `/visible` client (S12, T6's contract) and the per-compute connection budget (P8).

mod fakes;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::post;
use chelabase_query::realtime::key_string;
use fakes::*;
use realtime::check::{
    CONCURRENCY, CheckError, Limiter, VisibilityCache, WINDOW_DEADLINE, group_key,
};
use realtime::filter::{FilterSettings, TableFilter};
use realtime::merge::RelWindow;
use realtime::owner::{OwnerClient, convert_body, parse_convert, parse_visible, visible_body};
use realtime::pool::{ComputeBudget, PER_COMPUTE};
use realtime::protocol::{Change, ChangeOp};
use realtime::source::ChannelEvent;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::{Instant, advance};

fn window(inserts: &[&str], updates: &[&str], deletes: &[&str]) -> RelWindow {
    RelWindow {
        rel: REL,
        changed: false,
        inserts: inserts.iter().map(|k| texts(k)).collect(),
        updates: updates.iter().map(|k| texts(k)).collect(),
        deletes: deletes.iter().map(|k| texts(k)).collect(),
    }
}

fn keyed(op: ChangeOp, keys: &[&str]) -> Change {
    Change {
        op,
        keys: Some(keys.iter().map(|k| ks(k)).collect()),
    }
}

fn changed() -> ChannelEvent {
    ChannelEvent::Rows(vec![Change {
        op: ChangeOp::Changed,
        keys: None,
    }])
}

struct Fx {
    checker: Arc<FakeChecker>,
    limiter: Arc<Limiter>,
    cache: Arc<VisibilityCache>,
    filter: TableFilter,
}

fn fx_with(rel: chelabase_query::realtime::Relation, bound: usize) -> Fx {
    let checker = FakeChecker::new();
    let limiter = Limiter::new(bound);
    let cache = VisibilityCache::new(Duration::from_millis(100));
    let filter = TableFilter::new(
        target(),
        rel,
        checker.clone(),
        limiter.clone(),
        cache.clone(),
        FilterSettings::default(),
    );
    Fx {
        checker,
        limiter,
        cache,
        filter,
    }
}

fn fx() -> Fx {
    fx_with(relation(REL, "items"), CONCURRENCY)
}

impl Fx {
    fn viewer(
        &mut self,
        id: u64,
        who: realtime::source::CheckIdentity,
        bypass: bool,
    ) -> mpsc::Receiver<ChannelEvent> {
        let (tx, rx) = mpsc::channel(256);
        self.filter.add(id, who, bypass, tx);
        rx
    }
    async fn deliver(&mut self, w: &RelWindow, xids: &[u64]) {
        self.filter.deliver(w, xids, Instant::now()).await;
    }
}

#[tokio::test(start_paused = true)]
async fn bypass_viewers_get_every_existing_key_and_keyed_deletes() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "2", "3"]);
    let mut rx = f.viewer(1, role("billing"), true);
    f.deliver(&window(&["1", "2"], &["3"], &["4"]), &[10]).await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![
            keyed(ChangeOp::Insert, &["1", "2"]),
            keyed(ChangeOp::Update, &["3"]),
            keyed(ChangeOp::Delete, &["4"]),
        ])],
        "one rows event, ops in order, keys as key strings (S11)"
    );
}

#[tokio::test(start_paused = true)]
async fn only_keys_the_check_returned_are_delivered() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "3"]);
    f.checker.hide(Some("reader"), &["3"]);
    let mut bypass = f.viewer(1, role("billing"), true);
    let mut rls = f.viewer(2, role("reader"), false);
    f.deliver(&window(&["1", "2", "3"], &[], &[]), &[10]).await;
    assert_eq!(
        drain(&mut rls),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &["1"])])],
        "3 is hidden by RLS, 2 no longer exists"
    );
    assert_eq!(
        drain(&mut bypass),
        vec![ChannelEvent::Rows(vec![keyed(
            ChangeOp::Insert,
            &["1", "3"]
        )])],
        "a bypass viewer still gets only rows that exist when checked"
    );
}

#[tokio::test(start_paused = true)]
async fn one_check_per_identity_group() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    let mut rxs = vec![];
    for id in 1..=3 {
        rxs.push(f.viewer(id, role("billing"), true));
    }
    rxs.push(f.viewer(4, role("reader"), true));
    // Two owner viewers (two members, two tokens): one owner group.
    rxs.push(f.viewer(5, owner("member-a"), true));
    rxs.push(f.viewer(6, owner("member-b"), true));
    f.deliver(&window(&["1"], &[], &[]), &[10]).await;
    assert_eq!(f.checker.checks_of(Some("billing")), 1);
    assert_eq!(f.checker.checks_of(Some("reader")), 1);
    assert_eq!(f.checker.checks_of(None), 1);
    for rx in rxs.iter_mut() {
        assert_eq!(
            drain(rx),
            vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &["1"])])]
        );
    }
    assert_eq!(group_key(&role("billing")), Some("billing".to_string()));
    assert_eq!(group_key(&owner("x")), None);
}

#[tokio::test(start_paused = true)]
async fn rls_deletes_only_for_keys_in_the_view_set_or_delivered_before() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["4", "5"]);
    f.checker.hide(Some("reader"), &[]);
    let mut a = f.viewer(1, role("reader"), false);
    let mut b = f.viewer(2, role("reader"), false);
    f.deliver(&window(&["5"], &[], &[]), &[10]).await;
    drain(&mut a);
    drain(&mut b);
    // 4 is visible to A's group when A reports it on screen.
    f.filter.view(1, vec![ks("4")]).await;
    f.checker.delete_rows(REL, &["4", "5"]);
    f.deliver(&window(&[], &[], &["4", "5", "6"]), &[11]).await;
    assert_eq!(
        drain(&mut a),
        vec![ChannelEvent::Rows(vec![keyed(
            ChangeOp::Delete,
            &["4", "5"]
        )])],
        "4 is on A's screen, 5 was delivered to A; 6 says nothing"
    );
    assert_eq!(
        drain(&mut b),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &["5"])])]
    );
}

#[tokio::test(start_paused = true)]
async fn an_rls_viewer_gets_nothing_for_deletes_it_never_saw() {
    let mut f = fx();
    f.checker.hide(Some("reader"), &[]);
    let mut rx = f.viewer(1, role("reader"), false);
    f.deliver(&window(&[], &[], &["9"]), &[10]).await;
    assert_eq!(f.filter.is_bypass(1), Some(false));
    assert_eq!(drain(&mut rx), vec![], "no event at all: nothing to send");
}

#[tokio::test(start_paused = true)]
async fn the_view_set_is_replaced_on_each_send() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "2", "3"]);
    f.checker.hide(Some("reader"), &[]);
    let mut rx = f.viewer(1, role("reader"), false);
    f.filter.view(1, vec![ks("1"), ks("2")]).await;
    f.filter.view(1, vec![ks("3")]).await;
    f.checker.delete_rows(REL, &["1", "3"]);
    f.deliver(&window(&[], &[], &["1", "3"]), &[10]).await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &["3"])])]
    );
}

#[tokio::test(start_paused = true)]
async fn deletes_are_keyed_in_the_sessions_key_form() {
    let mut f = fx_with(
        relation_with(REL, "events", vec![col("at", "timestamptz")]),
        CONCURRENCY,
    );
    // The plugin prints UTC; the viewer's session (role TimeZone Asia/Tokyo) prints +09.
    f.checker
        .convert_as(&["2026-01-02 03:04:05+00"], &ks("2026-01-02 12:04:05+09"));
    let mut rx = f.viewer(1, role("billing"), true);
    f.deliver(&window(&[], &[], &["2026-01-02 03:04:05+00"]), &[10])
        .await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(
            ChangeOp::Delete,
            &["2026-01-02 12:04:05+09"]
        )])],
        "the key Studio has on its page"
    );
    assert!(
        f.checker
            .calls()
            .iter()
            .any(|c| matches!(c, Call::Convert { role: Some(r), .. } if r == "billing")),
        "converted in the group's own session"
    );
}

#[tokio::test(start_paused = true)]
async fn an_update_that_hides_a_viewed_row_is_a_keyed_delete() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["7"]);
    f.checker.hide(Some("reader"), &[]);
    let mut a = f.viewer(1, role("reader"), false);
    let mut b = f.viewer(2, role("reader"), false);
    // Visible when A reports it on screen; then an update hides it.
    f.filter.view(1, vec![ks("7")]).await;
    f.checker.hide(Some("reader"), &["7"]);
    f.deliver(&window(&[], &["7"], &[]), &[10]).await;
    assert_eq!(
        drain(&mut a),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &["7"])])],
        "W26: A saw the row, now it's hidden"
    );
    assert_eq!(drain(&mut b), vec![], "B never saw it: nothing");
}

// R19 as ruled ("only for keys seen visible"): a client can't earn a keyed delete by guessing.

#[tokio::test(start_paused = true)]
async fn a_guessed_hidden_key_in_view_gets_nothing_on_delete() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "8"]);
    f.checker.hide(Some("reader"), &["8"]);
    let mut rx = f.viewer(1, role("reader"), false);
    // 1 is visible (a control), 8 is hidden: the client guessed it.
    f.filter.view(1, vec![ks("1"), ks("8")]).await;
    f.checker.delete_rows(REL, &["1", "8"]);
    f.deliver(&window(&[], &[], &["1", "8"]), &[10]).await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &["1"])])],
        "the hidden row's delete says nothing: its existence never leaks"
    );
}

#[tokio::test(start_paused = true)]
async fn a_guessed_hidden_key_in_view_gets_nothing_on_an_update_that_hides_it() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "8"]);
    f.checker.hide(Some("reader"), &["8"]);
    let mut rx = f.viewer(1, role("reader"), false);
    f.filter.view(1, vec![ks("1"), ks("8")]).await;
    f.checker.hide(Some("reader"), &["1"]);
    f.deliver(&window(&[], &["1", "8"], &[]), &[10]).await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &["1"])])],
        "W26 for the key seen visible only"
    );
}

#[tokio::test(start_paused = true)]
async fn a_view_frame_costs_one_check_and_a_failed_check_confirms_nothing() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "2", "3"]);
    f.checker.hide(Some("reader"), &[]);
    let mut rx = f.viewer(1, role("reader"), false);
    f.filter.view(1, vec![ks("1"), ks("2"), ks("3")]).await;
    assert_eq!(
        f.checker.checks_of(Some("reader")),
        1,
        "one check per view frame"
    );
    f.checker
        .fail_checks(Some("reader"), Some(CheckError::Timeout));
    advance(Duration::from_millis(150)).await; // past the R12 cache
    f.filter.view(1, vec![ks("2")]).await;
    f.checker.fail_checks(Some("reader"), None);
    f.checker.delete_rows(REL, &["1", "2"]);
    f.deliver(&window(&[], &[], &["1", "2"]), &[10]).await;
    assert_eq!(
        drain(&mut rx),
        vec![],
        "the failed frame replaced the view set with nothing confirmed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_bypass_group_is_reclassified_before_anything_is_released() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "2"]);
    // Joined as bypass; since then `ALTER ROLE billing NOBYPASSRLS` (no event): the window's
    // own probe says RLS.
    let mut rx = f.viewer(1, role("billing"), true);
    f.checker.hide(Some("billing"), &["2"]);
    f.deliver(&window(&["1", "2"], &[], &["8"]), &[10]).await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &["1"])])],
        "no hidden insert, no delete of a key the viewer never saw"
    );
    assert_eq!(f.filter.is_bypass(1), Some(false));
}

#[tokio::test(start_paused = true)]
async fn a_failed_probe_is_changed() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    let mut rx = f.viewer(1, role("billing"), true);
    // REVOKE USAGE ON SCHEMA / SELECT since the join (W2).
    f.checker.fail_probe(Some("billing"), true);
    f.deliver(&window(&["1"], &[], &["2"]), &[10]).await;
    assert_eq!(
        drain(&mut rx),
        vec![changed()],
        "never keys, deletes included"
    );
}

#[tokio::test(start_paused = true)]
async fn a_check_error_or_timeout_is_changed() {
    for err in [
        CheckError::Timeout,
        CheckError::Failed("permission denied for type".into()),
        CheckError::Unavailable("compute".into()),
        CheckError::OwnerLevel,
    ] {
        let mut f = fx();
        f.checker.insert_rows(REL, &["1"]);
        let mut rx = f.viewer(1, role("billing"), true);
        let mut other = f.viewer(2, role("reader"), true);
        f.checker.fail_checks(Some("billing"), Some(err.clone()));
        f.deliver(&window(&["1"], &[], &["2"]), &[10]).await;
        assert_eq!(drain(&mut rx), vec![changed()], "{err:?}");
        assert_eq!(
            drain(&mut other),
            vec![ChannelEvent::Rows(vec![
                keyed(ChangeOp::Insert, &["1"]),
                keyed(ChangeOp::Delete, &["2"])
            ])],
            "only the failing group degrades"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_changed_relation_is_changed_for_everyone_without_a_check() {
    let mut f = fx();
    let mut a = f.viewer(1, role("billing"), true);
    let mut b = f.viewer(2, role("reader"), false);
    let mut w = window(&[], &[], &[]);
    w.changed = true;
    f.deliver(&w, &[10]).await;
    assert_eq!(drain(&mut a), vec![changed()]);
    assert_eq!(drain(&mut b), vec![changed()]);
    assert_eq!(f.checker.checks().len(), 0);
}

#[tokio::test(start_paused = true)]
async fn unsupported_key_types_and_nan_texts_are_never_sent_to_a_check() {
    // A float8 key with a NaN text (P6: NaN never equals itself).
    let mut f = fx_with(
        relation_with(REL, "m", vec![col("x", "float8")]),
        CONCURRENCY,
    );
    let mut rx = f.viewer(1, role("billing"), true);
    f.deliver(&window(&["1.5", "NaN"], &[], &[]), &[10]).await;
    assert_eq!(drain(&mut rx), vec![changed()]);
    assert!(
        f.checker.checks().iter().all(|c| !matches!(
            c,
            Call::Check { keys: Some(k), .. } if k.iter().any(|t| t[0] == "NaN")
        )),
        "{:?}",
        f.checker.checks()
    );

    // A money key (input depends on lc_monetary): the relation is always changed.
    let mut f = fx_with(
        relation_with(REL, "p", vec![col("price", "money")]),
        CONCURRENCY,
    );
    let mut rx = f.viewer(1, role("billing"), true);
    f.deliver(&window(&["$1.00"], &[], &["$2.00"]), &[10]).await;
    assert_eq!(drain(&mut rx), vec![changed()]);
    assert_eq!(f.checker.checks().len(), 0);
}

#[tokio::test(start_paused = true)]
async fn every_xid_of_the_window_goes_with_the_check() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    let mut rx = f.viewer(1, role("billing"), true);
    f.deliver(&window(&["1"], &[], &[]), &[5, 6]).await;
    assert!(
        matches!(f.checker.checks().first(), Some(Call::Check { xids, .. }) if xids == &vec![5, 6]),
        "{:?}",
        f.checker.checks()
    );
    assert_eq!(drain(&mut rx).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn visibility_is_polled_every_50_ms_until_visible() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    f.checker.invisible_for(42, 3);
    let mut rx = f.viewer(1, role("billing"), true);
    let started = Instant::now();
    f.deliver(&window(&["1"], &[], &[]), &[42]).await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &["1"])])],
        "visible at the 4th poll"
    );
    assert_eq!(f.checker.checks_of(Some("billing")), 4);
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(150) && took < Duration::from_millis(400),
        "{took:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_never_visible_xid_is_changed_after_2_s() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    f.checker.never_visible(42);
    let mut rx = f.viewer(1, role("billing"), true);
    let started = Instant::now();
    f.deliver(&window(&["1"], &[], &[]), &[42]).await;
    assert_eq!(drain(&mut rx), vec![changed()]);
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(2) && took < Duration::from_millis(2_200),
        "{took:?}"
    );
    let polls = f.checker.checks_of(Some("billing"));
    assert!((35..=45).contains(&polls), "~40 polls at 50 ms: {polls}");
}

#[tokio::test(start_paused = true)]
async fn the_permit_is_released_between_polls() {
    let mut f = fx_with(relation(REL, "items"), 1);
    f.checker.insert_rows(REL, &["1"]);
    f.checker.invisible_for(42, 2);
    let mut rx = f.viewer(1, role("billing"), true);
    let limiter = f.limiter.clone();
    let checker = f.checker.clone();
    let task = tokio::spawn(async move {
        f.deliver(&window(&["1"], &[], &[]), &[42]).await;
        f
    });
    // Until the first poll has answered "not visible yet" (paused time doesn't move while
    // this task keeps yielding).
    let mut spins = 0;
    while (checker.checks().is_empty() || checker.in_flight() > 0) && spins < 10_000 {
        tokio::task::yield_now().await;
        spins += 1;
    }
    assert_eq!(checker.checks().len(), 1, "the first poll ran");
    advance(Duration::from_millis(10)).await;
    assert_eq!(
        limiter.available(),
        1,
        "nothing held while waiting for the next poll"
    );
    let _f = task.await.unwrap();
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &["1"])])]
    );
}

#[tokio::test(start_paused = true)]
async fn more_groups_than_the_bound_overflow_to_changed() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    let mut rxs = vec![];
    for i in 0..9u64 {
        rxs.push(f.viewer(i, role(&format!("r{i}")), true));
    }
    f.checker.hold();
    let checker = f.checker.clone();
    let task = tokio::spawn(async move {
        f.deliver(&window(&["1"], &[], &[]), &[10]).await;
        f
    });
    let mut waited = Duration::ZERO;
    while checker.in_flight() < CONCURRENCY && waited < Duration::from_secs(1) {
        tokio::time::sleep(Duration::from_millis(5)).await;
        waited += Duration::from_millis(5);
    }
    assert_eq!(
        checker.in_flight(),
        CONCURRENCY,
        "groups are checked concurrently, up to the bound"
    );
    checker.release();
    let _f = task.await.unwrap();
    assert!(checker.max_in_flight() <= CONCURRENCY);
    let mut keyed_n = 0;
    let mut changed_n = 0;
    for rx in rxs.iter_mut() {
        match drain(rx).as_slice() {
            [ev] if *ev == changed() => changed_n += 1,
            [ChannelEvent::Rows(c)] if c == &vec![keyed(ChangeOp::Insert, &["1"])] => keyed_n += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((keyed_n, changed_n), (8, 1));
}

#[tokio::test(start_paused = true)]
async fn a_window_unresolved_5_s_after_it_closed_is_changed() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    let mut rx = f.viewer(1, role("billing"), true);
    f.checker.hold();
    let closed_at = Instant::now();
    let started = Instant::now();
    f.filter
        .deliver(&window(&["1"], &[], &[]), &[10], closed_at)
        .await;
    assert_eq!(drain(&mut rx), vec![changed()]);
    let took = started.elapsed();
    assert!(
        took >= WINDOW_DEADLINE && took < WINDOW_DEADLINE + Duration::from_millis(200),
        "{took:?}"
    );
    // The deadline counts from the window's close, not from when its turn came.
    f.checker.release();
    f.checker.hold();
    let closed_at = Instant::now() - Duration::from_millis(4_900);
    let started = Instant::now();
    f.filter
        .deliver(&window(&["1"], &[], &[]), &[11], closed_at)
        .await;
    assert_eq!(drain(&mut rx), vec![changed()]);
    assert!(
        started.elapsed() <= Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    f.checker.release();
}

#[tokio::test(start_paused = true)]
async fn sinks_are_written_with_try_send_and_never_awaited() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "2", "3"]);
    let (tx, mut rx) = mpsc::channel(1);
    f.filter.add(1, role("billing"), true, tx);
    f.deliver(&window(&["1"], &[], &[]), &[10]).await;
    assert_eq!(rx.len(), 1, "the first window fills the sink");
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        f.filter
            .deliver(&window(&["2"], &[], &[]), &[11], Instant::now()),
    )
    .await;
    assert!(second.is_ok(), "a full sink never blocks the filter");
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &["1"])])]
    );
    // The next delivery announces the drop: a resync (the client refetches everything shown,
    // which covers this window too, already committed).
    f.deliver(&window(&["3"], &[], &[]), &[12]).await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Resync {
            reason: "overflow".into()
        }],
        "the dropped window is announced before anything else"
    );
    f.deliver(&window(&["3"], &[], &[]), &[13]).await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &["3"])])],
        "then rows as usual"
    );
}

#[tokio::test(start_paused = true)]
async fn a_removed_viewer_gets_nothing() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    let mut rx = f.viewer(1, role("billing"), true);
    let mut stays = f.viewer(2, role("reader"), true);
    f.filter.remove(1);
    f.deliver(&window(&["1"], &[], &[]), &[10]).await;
    assert_eq!(
        drain(&mut stays),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &["1"])])]
    );
    assert_eq!(drain(&mut rx), vec![]);
    assert_eq!(
        f.checker.checks_of(Some("billing")),
        0,
        "no viewer left in the group: no check"
    );
}

// --- R15 / R12: presence visibility through the cache ---

#[tokio::test(start_paused = true)]
async fn presence_visibility_is_the_groups_and_cached_for_one_window_at_most() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1", "2"]);
    f.checker.hide(Some("reader"), &["2"]);
    let _rx = f.viewer(1, role("reader"), false);
    let keys = vec![ks("1"), ks("2")];
    assert_eq!(f.filter.visible(1, &keys).await, Ok(vec![true, false]));
    assert_eq!(f.filter.visible(1, &keys).await, Ok(vec![true, false]));
    assert_eq!(
        f.checker.checks_of(Some("reader")),
        1,
        "cached within the window"
    );
    advance(Duration::from_millis(150)).await;
    let _ = f.filter.visible(1, &keys).await;
    assert_eq!(f.checker.checks_of(Some("reader")), 2, "at most one window");
    f.cache.flush();
    let _ = f.filter.visible(1, &keys).await;
    assert_eq!(f.checker.checks_of(Some("reader")), 3, "flushed");
}

#[tokio::test(start_paused = true)]
async fn a_delivered_window_flushes_the_cache() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    let _rx = f.viewer(1, role("reader"), true);
    let _ = f.filter.visible(1, &[ks("1")]).await;
    f.deliver(&window(&[], &["1"], &[]), &[10]).await;
    f.checker.clear_calls();
    let _ = f.filter.visible(1, &[ks("1")]).await;
    assert_eq!(
        f.checker.checks_of(Some("reader")),
        1,
        "re-checked after the window"
    );
}

#[tokio::test(start_paused = true)]
async fn presence_visibility_fails_closed() {
    let mut f = fx();
    f.checker.insert_rows(REL, &["1"]);
    let _rx = f.viewer(1, role("reader"), true);
    f.checker
        .fail_checks(Some("reader"), Some(CheckError::Timeout));
    assert_eq!(f.filter.visible(1, &[ks("1")]).await, Err(()));
    assert_eq!(
        f.filter.visible(99, &[ks("1")]).await,
        Err(()),
        "unknown viewer"
    );
    f.checker.fail_checks(Some("reader"), None);
    f.checker.fail_probe(Some("reader"), true);
    assert_eq!(
        f.filter.visible(1, &[ks("1")]).await,
        Err(()),
        "failed probe"
    );
}

#[tokio::test(start_paused = true)]
async fn presence_visibility_parses_key_strings_for_the_check() {
    let mut f = fx_with(
        relation_with(REL, "c", vec![col("a", "int4"), col("b", "text")]),
        CONCURRENCY,
    );
    let _rx = f.viewer(1, role("reader"), true);
    let key = key_string(&["1".to_string(), "x\"y".to_string()]);
    let _ = f.filter.visible(1, &[key, "not json".into()]).await;
    let sent: Vec<Vec<Vec<String>>> = f
        .checker
        .checks()
        .into_iter()
        .filter_map(|c| match c {
            Call::Check { keys: Some(k), .. } => Some(k),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        vec![vec![vec!["1".to_string(), "x\"y".to_string()]]],
        "a malformed key string is never sent (not visible)"
    );
}

#[test]
fn the_cache_holds_one_window() {
    let cache = VisibilityCache::new(Duration::from_millis(100));
    let t0 = Instant::now();
    let g = Some("reader".to_string());
    assert_eq!(cache.get(t0, &g, REL, "k"), None);
    cache.put(t0, &g, REL, "k", false);
    assert_eq!(
        cache.get(t0 + Duration::from_millis(99), &g, REL, "k"),
        Some(false)
    );
    assert_eq!(
        cache.get(t0 + Duration::from_millis(100), &g, REL, "k"),
        None
    );
    cache.put(t0, &g, REL, "k", true);
    assert_eq!(cache.get(t0, &None, REL, "k"), None, "per group");
    assert_eq!(cache.get(t0, &g, REL + 1, "k"), None, "per relation");
    cache.flush();
    assert_eq!(cache.get(t0, &g, REL, "k"), None);
}

#[test]
fn the_limiter_bounds_checks() {
    let l = Limiter::new(2);
    let a = l.try_permit();
    let b = l.try_permit();
    assert!(a.is_some() && b.is_some());
    assert_eq!(l.available(), 0);
    assert!(l.try_permit().is_none(), "overflow");
    drop(a);
    assert_eq!(l.available(), 1);
    assert!(l.try_permit().is_some());
    assert_eq!(CONCURRENCY, 8);
}

// --- the owner's /visible (S12) ---

#[test]
fn the_visible_request_body() {
    let body = visible_body(
        "member-token",
        &target(),
        REL,
        Some(4_294_967_301),
        Some(&[texts("1"), texts("2")]),
    );
    assert_eq!(
        body,
        json!({
            "token": "member-token",
            "database_id": DB,
            "branch_id": BRANCH,
            "pg_database": "app",
            "rel_oid": REL,
            "xid": "4294967301",
            "keys": [["1"], ["2"]],
        }),
        "xid as an xid8 decimal string"
    );
    let probe = visible_body("t", &target(), REL, None, None);
    assert_eq!(
        (probe["xid"].clone(), probe["keys"].clone()),
        (Value::Null, Value::Null)
    );
}

#[test]
fn the_visible_answer() {
    let ok = parse_visible(
        200,
        br#"{"probe":{"ok":true,"bypass":true},"visible":true,"keys":["[\"1\"]"]}"#,
    );
    assert_eq!(
        ok,
        Ok(realtime::check::CheckAnswer {
            probe_ok: true,
            bypass: true,
            visible: true,
            keys: vec![ks("1")],
            converted: vec![],
        })
    );
    let denied = parse_visible(
        200,
        br#"{"probe":{"ok":false,"bypass":false},"visible":true,"keys":[]}"#,
    )
    .unwrap();
    assert!(!denied.probe_ok);
    for (status, body) in [
        (400u16, &b"{}"[..]),
        (401, b""),
        (403, b"{}"),
        (500, b"{}"),
        (503, b"{}"),
        (200, b"not json"),
        (200, br#"{"visible":true}"#),
    ] {
        assert!(
            matches!(parse_visible(status, body), Err(CheckError::Unavailable(_))),
            "{status} {}",
            String::from_utf8_lossy(body)
        );
    }
}

#[tokio::test]
async fn the_owner_client_posts_with_realtimes_bearer() {
    let seen: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Arc::default();
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/internal/realtime/visible",
        post(move |headers: HeaderMap, body: axum::Json<Value>| {
            let seen = seen2.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                seen.lock().unwrap().push((auth, body.0));
                axum::Json(
                    json!({"probe": {"ok": true, "bypass": true}, "visible": false, "keys": []}),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = OwnerClient::new(&format!("http://{addr}"), "rt-bearer");
    let answer = client
        .visible("member-token", &target(), REL, Some(7), Some(&[texts("1")]))
        .await;
    assert_eq!(
        answer.map(|a| a.visible),
        Ok(false),
        "visible: false = not yet in the snapshot"
    );
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "one attempt per call");
        assert_eq!(seen[0].0, "Bearer rt-bearer");
        assert_eq!(seen[0].1["xid"], "7");
        assert_eq!(seen[0].1["token"], "member-token");
    }

    // Nobody listening: unavailable.
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let gone = listener.local_addr().unwrap();
    drop(listener);
    let client = OwnerClient::new(&format!("http://{gone}"), "rt-bearer");
    assert!(matches!(
        client.visible("t", &target(), REL, None, None).await,
        Err(CheckError::Unavailable(_))
    ));
}

// --- P8: the per-compute connection budget ---

#[test]
fn the_budget_is_per_compute_across_its_databases() {
    let budget = ComputeBudget::new(PER_COMPUTE);
    let held: Vec<_> = (0..PER_COMPUTE)
        .filter_map(|_| budget.try_take(BRANCH))
        .collect();
    assert_eq!(held.len(), 4, "four connections per compute per instance");
    assert!(budget.try_take(BRANCH).is_none());
    assert_eq!(budget.in_use(BRANCH), 4);
    let other = uuid::Uuid::from_u128(9);
    assert!(budget.try_take(other).is_some(), "another compute");
    drop(held);
    assert_eq!(budget.in_use(BRANCH), 0);
    let _ = HashSet::<u8>::new();
}

// --- owner deletes: `/visible`'s convert mode (coordinator ruling) ---

#[tokio::test(start_paused = true)]
async fn owner_deletes_are_converted_in_the_owners_session() {
    let mut f = fx_with(
        relation_with(REL, "events", vec![col("at", "timestamptz")]),
        CONCURRENCY,
    );
    // The owner's session (database-level TimeZone Asia/Tokyo) prints +09.
    f.checker
        .convert_as(&["2026-01-02 03:04:05+00"], &ks("2026-01-02 12:04:05+09"));
    let mut rx = f.viewer(1, owner("member-a"), true);
    f.deliver(&window(&[], &[], &["2026-01-02 03:04:05+00"]), &[10])
        .await;
    assert_eq!(
        drain(&mut rx),
        vec![ChannelEvent::Rows(vec![keyed(
            ChangeOp::Delete,
            &["2026-01-02 12:04:05+09"]
        )])],
        "the owner's page key, not the plugin's UTC text"
    );
    assert!(
        f.checker.calls().iter().any(|c| matches!(
            c,
            Call::Convert { role: None, keys, .. } if keys == &vec![texts("2026-01-02 03:04:05+00")]
        )),
        "{:?}",
        f.checker.calls()
    );
}

#[test]
fn the_convert_request_body() {
    let body = convert_body("member-token", &target(), REL, &[texts("1"), texts("2")]);
    assert_eq!(
        body,
        json!({
            "token": "member-token",
            "database_id": DB,
            "branch_id": BRANCH,
            "pg_database": "app",
            "rel_oid": REL,
            "xid": null,
            "keys": [["1"], ["2"]],
            "convert": true,
        })
    );
}

#[test]
fn the_convert_answer() {
    assert_eq!(
        parse_convert(200, br#"{"keys":["[\"1\"]","[\"2\"]"]}"#, 2),
        Ok(vec![ks("1"), ks("2")])
    );
    for (status, body, n) in [
        (200u16, &br#"{"keys":["[\"1\"]"]}"#[..], 2usize),
        (400, b"{}", 1),
        (503, b"{}", 1),
        (200, b"nope", 1),
        (200, br#"{"keys":[1]}"#, 1),
    ] {
        assert!(
            matches!(
                parse_convert(status, body, n),
                Err(CheckError::Unavailable(_))
            ),
            "{status} {} ({n} asked)",
            String::from_utf8_lossy(body)
        );
    }
}

#[tokio::test]
async fn the_owner_client_converts_with_realtimes_bearer() {
    let seen: Arc<std::sync::Mutex<Vec<(String, Value)>>> = Arc::default();
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/internal/realtime/visible",
        post(move |headers: HeaderMap, body: axum::Json<Value>| {
            let seen = seen2.clone();
            async move {
                let auth = headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                seen.lock().unwrap().push((auth, body.0));
                axum::Json(json!({"keys": [ks("2026-01-02 12:04:05+09")]}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = OwnerClient::new(&format!("http://{addr}"), "rt-bearer");
    let got = client
        .convert(
            "member-token",
            &target(),
            REL,
            &[texts("2026-01-02 03:04:05+00")],
        )
        .await;
    assert_eq!(got, Ok(vec![ks("2026-01-02 12:04:05+09")]));
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "Bearer rt-bearer");
    assert_eq!(seen[0].1["convert"], true);
}
