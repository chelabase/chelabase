//! Object storage m17: the `storage:` channel (Studio's object browser). Parsing and the
//! `table:` refusal of the `storage` schema (AC 3), the bucket filter (AC 5), the per-viewer
//! read check dropping ids the viewer's role can't read (AC 2), and the socket's join,
//! authorization and revocation of a storage channel (AC 4).

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;
mod fakes;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chelabase_query::realtime::Relation;
use fakes::*;
use proptest::prelude::*;
use realtime::channel::{JoinTarget, TableChannel, parse_join};
use realtime::check::{Limiter, VisibilityCache};
use realtime::filter::{FilterSettings, TableFilter};
use realtime::merge::RelWindow;
use realtime::protocol::{Change, ChangeOp};
use realtime::source::ChannelEvent;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use uuid::Uuid;

const BUCKET_A: Uuid = Uuid::from_u128(0xaaaa_aaaa_aaaa_4aaa_8aaa_aaaa_aaaa_aaaa);
const BUCKET_B: Uuid = Uuid::from_u128(0xbbbb_bbbb_bbbb_4bbb_8bbb_bbbb_bbbb_bbbb);
// Object ids (the key of `storage.objects`).
const O1: &str = "00000000-0000-4000-8000-000000000001";
const O2: &str = "00000000-0000-4000-8000-000000000002";
const O3: &str = "00000000-0000-4000-8000-000000000003";

fn storage_params(role: Value) -> Value {
    json!({"database_id": common::DB, "branch_id": common::BR, "pg_database": "app",
        "bucket_id": BUCKET_A, "role": role})
}

// --- parsing (AC 3) --------------------------------------------------------------------------

#[test]
fn storage_join_watches_storage_objects_in_one_bucket() {
    let target = parse_join("storage", Some(&storage_params(json!("authenticated")))).unwrap();
    let JoinTarget::Table { channel, role } = &target else {
        panic!("a storage join is a scoped table watch: {target:?}");
    };
    assert_eq!(
        channel,
        &TableChannel {
            database_id: common::DB,
            branch_id: common::BR,
            pg_database: "app".into(),
            schema: "storage".into(),
            table: "objects".into(),
            bucket: Some(BUCKET_A),
        }
    );
    assert_eq!(role.as_deref(), Some("authenticated"));
    let owner = parse_join("storage", Some(&storage_params(json!(null)))).unwrap();
    assert!(
        matches!(owner, JoinTarget::Table { role: None, .. }),
        "role null is owner mode"
    );
}

#[test]
fn storage_channel_id_is_compact_json_with_the_bucket() {
    let target = parse_join("storage", Some(&storage_params(json!("authenticated")))).unwrap();
    let want = format!(
        r#"storage:["{}","{}","app","{BUCKET_A}"]"#,
        common::DB,
        common::BR
    );
    assert_eq!(target.id(), want);
    let owner = parse_join("storage", Some(&storage_params(json!(null)))).unwrap();
    assert_eq!(
        owner.id(),
        want,
        "one channel id per bucket, whatever the role"
    );
}

#[test]
fn bad_storage_params_are_refused() {
    let mut cases: Vec<Value> = vec![];
    for field in [
        "database_id",
        "branch_id",
        "pg_database",
        "bucket_id",
        "role",
    ] {
        let mut p = storage_params(json!("authenticated"));
        p.as_object_mut().unwrap().remove(field);
        cases.push(p);
    }
    let mut p = storage_params(json!("authenticated"));
    p["bucket_id"] = json!("images");
    cases.push(p);
    let mut p = storage_params(json!(""));
    p["role"] = json!("");
    cases.push(p);
    // A storage join names a bucket, never a schema or a table.
    for (field, value) in [("schema", "public"), ("table", "items")] {
        let mut p = storage_params(json!(null));
        p[field] = json!(value);
        cases.push(p);
    }
    for p in cases {
        assert!(parse_join("storage", Some(&p)).is_err(), "{p}");
    }
    assert!(parse_join("storage", None).is_err(), "params are required");
}

#[test]
fn table_join_of_the_storage_schema_is_refused() {
    for table in ["objects", "buckets", "upload_sessions"] {
        let p = json!({"database_id": common::DB, "branch_id": common::BR, "pg_database": "app",
            "schema": "storage", "table": table, "role": null});
        let err = parse_join("table", Some(&p)).unwrap_err();
        assert!(err.contains("storage"), "{err}");
    }
    // Only the schema named exactly `storage`.
    let p = json!({"database_id": common::DB, "branch_id": common::BR, "pg_database": "app",
        "schema": "storage_app", "table": "objects", "role": null});
    assert!(parse_join("table", Some(&p)).is_ok());
}

// --- the bucket filter and the read check (AC 2, AC 5) ----------------------------------------

/// `storage.objects` with its `uuid` key `id`.
fn objects() -> Relation {
    Relation {
        oid: REL,
        schema: "storage".into(),
        name: "objects".into(),
        key: vec![col("id", "uuid")],
    }
}

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

struct Fx {
    checker: Arc<FakeChecker>,
    filter: TableFilter,
}

/// A filter of `storage.objects` scoped to bucket A; O1 and O3 are in A, O2 in B.
fn fx() -> Fx {
    let checker = FakeChecker::new();
    checker.insert_rows(REL, &[O1, O2, O3]);
    checker.place(&[O1, O3], BUCKET_A);
    checker.place(&[O2], BUCKET_B);
    let mut filter = TableFilter::new(
        target(),
        objects(),
        checker.clone(),
        Limiter::new(8),
        VisibilityCache::new(Duration::from_millis(100)),
        FilterSettings::default(),
    );
    filter.set_bucket(Some(BUCKET_A));
    Fx { checker, filter }
}

impl Fx {
    fn viewer(
        &mut self,
        id: u64,
        who: realtime::source::CheckIdentity,
        bypass: bool,
    ) -> mpsc::Receiver<ChannelEvent> {
        let (tx, rx) = mpsc::channel(64);
        self.filter.add(id, who, bypass, tx);
        rx
    }
    async fn deliver(&mut self, w: &RelWindow) {
        self.filter.deliver(w, &[10], Instant::now()).await;
    }
}

#[tokio::test(start_paused = true)]
async fn storage_changes_in_other_buckets_never_reach_the_channel() {
    let mut f = fx();
    let mut owner = f.viewer(1, owner("member-a"), true);
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.deliver(&window(&[O1, O2], &[], &[])).await;
    assert_eq!(
        drain(&mut owner),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &[O1])])]
    );
    assert_eq!(
        drain(&mut reader),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &[O1])])]
    );
    // A window with only another bucket's changes sends nothing at all.
    f.deliver(&window(&[], &[O2], &[])).await;
    assert_eq!(drain(&mut owner), vec![]);
    assert_eq!(drain(&mut reader), vec![]);
    assert!(
        f.checker
            .check_buckets()
            .iter()
            .all(|b| *b == Some(BUCKET_A)),
        "every check is scoped to the channel's bucket: {:?}",
        f.checker.check_buckets()
    );
}

#[tokio::test(start_paused = true)]
async fn storage_ids_the_role_cannot_read_are_dropped() {
    let mut f = fx();
    f.checker.hide(Some("authenticated"), &[O3]);
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.deliver(&window(&[O1, O3], &[], &[])).await;
    assert_eq!(
        drain(&mut reader),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &[O1])])]
    );
    // A window of hidden objects only: no key, no `changed`, nothing.
    f.deliver(&window(&[O3], &[O3], &[O3])).await;
    assert_eq!(drain(&mut reader), vec![], "nothing reaches the viewer");
}

#[tokio::test(start_paused = true)]
async fn storage_deletes_are_keyed_only_for_objects_the_viewer_was_shown() {
    let mut f = fx();
    // The owner bypasses RLS, but a deleted row has no bucket to check: owner mode is held to
    // what it was shown, like an RLS viewer.
    let mut owner = f.viewer(1, owner("member-a"), true);
    f.deliver(&window(&[O1], &[], &[])).await;
    drain(&mut owner);
    f.deliver(&window(&[], &[], &[O1, O2])).await;
    assert_eq!(
        drain(&mut owner),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &[O1])])],
        "O1 was delivered; O2 (bucket B) never was"
    );
    assert_eq!(
        f.filter.is_bypass(1),
        Some(false),
        "a bucket's groups are never bypass groups"
    );
    // An object on the viewer's screen (a confirmed `view`) gets its keyed delete too.
    f.filter.view(1, vec![ks(O3)]).await;
    f.deliver(&window(&[], &[], &[O3])).await;
    assert_eq!(
        drain(&mut owner),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &[O3])])]
    );
}

#[tokio::test(start_paused = true)]
async fn storage_view_of_another_buckets_object_confirms_nothing() {
    let mut f = fx();
    let mut owner = f.viewer(1, owner("member-a"), true);
    // A guessed id of bucket B on the screen never earns a keyed delete.
    f.filter.view(1, vec![ks(O2)]).await;
    f.deliver(&window(&[], &[], &[O2])).await;
    assert_eq!(drain(&mut owner), vec![]);
}

#[tokio::test(start_paused = true)]
async fn storage_object_moved_out_of_the_bucket_is_a_keyed_delete_on_screen() {
    let mut f = fx();
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.filter.view(2, vec![ks(O1)]).await;
    // W26: O1 moves to bucket B; the check no longer returns it.
    f.checker.place(&[O1], BUCKET_B);
    f.deliver(&window(&[], &[O1], &[])).await;
    assert_eq!(
        drain(&mut reader),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &[O1])])]
    );
}

// --- never across buckets or visibility (#129's white-room audit, B1 and B2) ------------------
//
// The invariant (`realtime::filter`'s module docs): a storage channel's frames are derived only
// from keys checked visible to that viewer, in that bucket, except a TRUNCATE.

/// A bucket-A filter on its own limiter and settings.
fn fx_with(limiter: Arc<Limiter>, settings: FilterSettings) -> Fx {
    let checker = FakeChecker::new();
    checker.insert_rows(REL, &[O1, O2, O3]);
    checker.place(&[O1, O3], BUCKET_A);
    checker.place(&[O2], BUCKET_B);
    let mut filter = TableFilter::new(
        target(),
        objects(),
        checker.clone(),
        limiter,
        VisibilityCache::new(Duration::from_millis(100)),
        settings,
    );
    filter.set_bucket(Some(BUCKET_A));
    Fx { checker, filter }
}

/// Every key of a viewer's events, per op; panics on anything keyless.
fn keys_by_op(events: &[ChannelEvent]) -> Vec<(ChangeOp, String)> {
    let mut out = vec![];
    for e in events {
        let ChannelEvent::Rows(changes) = e else {
            panic!("only rows reach a storage viewer: {e:?}");
        };
        for c in changes {
            let keys = c
                .keys
                .as_ref()
                .unwrap_or_else(|| panic!("a keyless frame reached a storage viewer: {c:?}"));
            for k in keys {
                out.push((c.op, k.clone()));
            }
        }
    }
    out
}

/// An object id from a number (bucket placement is the test's).
fn oid(n: usize) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

#[tokio::test(start_paused = true)]
async fn storage_bulk_change_in_another_bucket_reaches_nobody() {
    let mut f = fx();
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.filter.view(2, vec![ks(O1)]).await;
    // A keyless window (the plugin's `c`: past its key limit, or a TRUNCATE) of bucket B.
    f.checker.delete_rows(REL, &[O2]);
    let w = RelWindow {
        rel: REL,
        changed: true,
        inserts: vec![],
        updates: vec![],
        deletes: vec![],
    };
    f.deliver(&w).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "a bulk change in bucket B reached bucket A's viewer"
    );
}

#[tokio::test(start_paused = true)]
async fn storage_truncate_is_a_keyed_delete_of_what_is_on_screen() {
    let mut f = fx();
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.filter.view(2, vec![ks(O1)]).await;
    f.deliver(&window(&[O3], &[], &[])).await;
    drain(&mut reader);
    f.checker.delete_rows(REL, &[O1, O2, O3]);
    let w = RelWindow {
        rel: REL,
        changed: true,
        inserts: vec![],
        updates: vec![],
        deletes: vec![],
    };
    f.deliver(&w).await;
    assert_eq!(
        drain(&mut reader),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &[O1])])],
        "the objects on screen are re-checked; nothing keyless"
    );
}

#[tokio::test(start_paused = true)]
async fn storage_window_over_the_key_limit_is_checked_in_chunks() {
    let mut f = fx_with(
        Limiter::new(8),
        FilterSettings {
            max_keys: 2,
            ..FilterSettings::default()
        },
    );
    let ids: Vec<String> = (10..15).map(oid).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    f.checker.insert_rows(REL, &refs);
    f.checker.place(&[refs[0], refs[2], refs[4]], BUCKET_A);
    f.checker.place(&[refs[1], refs[3]], BUCKET_B);
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.deliver(&window(&refs, &[], &[])).await;
    let got = keys_by_op(&drain(&mut reader));
    let want: Vec<(ChangeOp, String)> = [refs[0], refs[2], refs[4]]
        .iter()
        .map(|k| (ChangeOp::Insert, ks(k)))
        .collect();
    assert_eq!(got, want);
    for c in f.checker.checks() {
        let fakes::Call::Check { keys, .. } = c else {
            unreachable!()
        };
        assert!(
            keys.is_none_or(|k| k.len() <= 2),
            "a chunk holds at most max_keys keys"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn storage_saturated_limiter_waits_and_carries_instead_of_changed() {
    let limiter = Limiter::new(1);
    let mut f = fx_with(limiter.clone(), FilterSettings::default());
    let mut reader = f.viewer(2, role("authenticated"), false);
    let held = limiter.try_permit().unwrap(); // another bucket's filter is checking
    f.deliver(&window(&[O1], &[O2], &[])).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "bucket B's change reached bucket A's viewer"
    );
    drop(held);
    // The next window checks what was carried: O1 (bucket A) arrives, O2 (bucket B) never.
    f.deliver(&window(&[O3], &[], &[])).await;
    assert_eq!(
        keys_by_op(&drain(&mut reader)),
        vec![(ChangeOp::Insert, ks(O1)), (ChangeOp::Insert, ks(O3))]
    );
}

#[tokio::test(start_paused = true)]
async fn storage_nine_groups_exhausting_check_slots_get_no_changed() {
    let mut f = fx();
    f.checker.hold();
    let mut sinks = vec![];
    for i in 0..9u64 {
        sinks.push(f.viewer(i + 1, role(&format!("role{i}")), false));
    }
    let checker = f.checker.clone();
    let mut filter = f.filter;
    let task = tokio::spawn(async move {
        filter
            .deliver(&window(&[O1], &[O2], &[]), &[10], Instant::now())
            .await;
        filter
    });
    for _ in 0..1000 {
        if checker.in_flight() == 8 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(checker.in_flight(), 8, "eight groups hold every check slot");
    checker.release();
    let _filter = task.await.unwrap();
    for (i, rx) in sinks.iter_mut().enumerate() {
        assert_eq!(
            drain(rx),
            vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &[O1])])],
            "viewer {i}: O1 only, never `changed`"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn storage_failed_check_carries_instead_of_changed() {
    let mut f = fx();
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.checker.fail_checks(
        Some("authenticated"),
        Some(realtime::check::CheckError::Timeout),
    );
    f.deliver(&window(&[O1, O2], &[], &[])).await;
    assert_eq!(drain(&mut reader), vec![]);
    f.checker.fail_checks(Some("authenticated"), None);
    f.checker.fail_probe(Some("authenticated"), true);
    f.deliver(&window(&[O3], &[], &[])).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "a failed probe sends nothing either"
    );
    f.checker.fail_probe(Some("authenticated"), false);
    f.deliver(&window(&[], &[], &[])).await;
    assert_eq!(
        keys_by_op(&drain(&mut reader)),
        vec![(ChangeOp::Insert, ks(O1)), (ChangeOp::Insert, ks(O3))]
    );
}

#[tokio::test(start_paused = true)]
async fn storage_carried_keys_are_bounded_newest_kept() {
    let limiter = Limiter::new(1);
    let mut f = fx_with(
        limiter.clone(),
        FilterSettings {
            max_keys: 1,
            ..FilterSettings::default()
        },
    );
    let ids: Vec<String> = (100..112).map(oid).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    f.checker.insert_rows(REL, &refs);
    f.checker.place(&refs, BUCKET_A);
    let mut reader = f.viewer(2, role("authenticated"), false);
    let held = limiter.try_permit().unwrap();
    for k in &refs {
        f.deliver(&window(&[k], &[], &[])).await;
    }
    drop(held);
    f.deliver(&window(&[], &[], &[])).await;
    let got = keys_by_op(&drain(&mut reader));
    // At most ten windows' worth of keys (10 × max_keys) wait; the oldest beyond that go.
    let want: Vec<(ChangeOp, String)> = refs[2..]
        .iter()
        .map(|k| (ChangeOp::Insert, ks(k)))
        .collect();
    assert_eq!(got, want);
}

#[tokio::test(start_paused = true)]
async fn storage_delete_after_move_to_another_bucket_reaches_nobody() {
    let mut f = fx();
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.deliver(&window(&[O1], &[], &[])).await;
    assert_eq!(
        drain(&mut reader),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Insert, &[O1])])]
    );
    f.checker.place(&[O1], BUCKET_B);
    f.deliver(&window(&[], &[O1], &[])).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "the move out sends nothing off screen"
    );
    f.checker.delete_rows(REL, &[O1]);
    f.deliver(&window(&[], &[], &[O1])).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "a delete in bucket B reached bucket A's viewer"
    );
}

#[tokio::test(start_paused = true)]
async fn storage_delete_of_an_object_hidden_since_delivery_reaches_nobody() {
    let mut f = fx();
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.deliver(&window(&[O1], &[], &[])).await;
    drain(&mut reader);
    f.checker.hide(Some("authenticated"), &[O1]);
    f.deliver(&window(&[], &[O1], &[])).await;
    assert_eq!(drain(&mut reader), vec![]);
    f.checker.delete_rows(REL, &[O1]);
    f.deliver(&window(&[], &[], &[O1])).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "a hidden object's delete reached the viewer"
    );
}

#[tokio::test(start_paused = true)]
async fn storage_object_on_screen_moved_away_is_deleted_once() {
    let mut f = fx();
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.filter.view(2, vec![ks(O1)]).await;
    f.checker.place(&[O1], BUCKET_B);
    f.deliver(&window(&[], &[O1], &[])).await;
    assert_eq!(
        drain(&mut reader),
        vec![ChannelEvent::Rows(vec![keyed(ChangeOp::Delete, &[O1])])]
    );
    f.checker.delete_rows(REL, &[O1]);
    f.deliver(&window(&[], &[], &[O1])).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "its later delete in bucket B is bucket B's"
    );
}

// --- a view check racing a window, a trimmed carry (#129's re-audit, B3 and B4) -----------------

#[tokio::test(start_paused = true)]
async fn storage_view_confirmed_before_a_move_lets_no_bucket_b_change_through() {
    let mut f = fx();
    let mut reader = f.viewer(2, role("authenticated"), false);
    // Studio's view frame: asked under the filter, checked without it.
    let ask = f.filter.ask_keys(2, &[ks(O1)], false).unwrap();
    let seen = ask.run().await;
    // Meanwhile O1 moves to bucket B, and that window is delivered.
    f.checker.place(&[O1], BUCKET_B);
    f.deliver(&window(&[], &[O1], &[])).await;
    assert_eq!(drain(&mut reader), vec![]);
    // The pre-move answer lands.
    f.filter.set_view(2, &ask, vec![ks(O1)], seen);
    f.deliver(&window(&[], &[O1], &[])).await;
    f.checker.delete_rows(REL, &[O1]);
    f.deliver(&window(&[], &[], &[O1])).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "bucket B's changes to O1 reached bucket A's viewer"
    );
}

#[tokio::test(start_paused = true)]
async fn storage_trimmed_move_out_lets_no_bucket_b_delete_through() {
    let limiter = Limiter::new(1);
    let mut f = fx_with(
        limiter.clone(),
        FilterSettings {
            max_keys: 1,
            ..FilterSettings::default()
        },
    );
    let ids: Vec<String> = (100..112).map(oid).collect();
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    f.checker.insert_rows(REL, &refs);
    f.checker.place(&refs, BUCKET_A);
    let mut reader = f.viewer(2, role("authenticated"), false);
    f.deliver(&window(&[O1], &[], &[])).await;
    drain(&mut reader);
    let held = limiter.try_permit().unwrap();
    f.checker.place(&[O1], BUCKET_B);
    // The move out is carried, then trimmed by later carried keys.
    f.deliver(&window(&[], &[O1], &[])).await;
    for k in &refs {
        f.deliver(&window(&[k], &[], &[])).await;
    }
    drop(held);
    f.deliver(&window(&[], &[], &[])).await;
    drain(&mut reader);
    f.checker.delete_rows(REL, &[O1]);
    f.deliver(&window(&[], &[], &[O1])).await;
    f.deliver(&window(&[], &[], &[])).await;
    assert_eq!(
        drain(&mut reader),
        vec![],
        "a delete in bucket B reached bucket A's viewer"
    );
}

// --- the invariant as a property: interleavings of bucket A/B changes, bulk windows, permit
// starvation, a carry that overflows and trims, and view checks racing delivered windows ---------

#[derive(Debug, Clone)]
enum Step {
    /// Create or change object `i`: its bucket (A when true) and whether the role's policy hides it.
    Put {
        i: usize,
        in_a: bool,
        hidden: bool,
    },
    Delete {
        i: usize,
    },
    /// The viewer's screen: the objects of the mask (only those confirmed visible count), asked,
    /// checked and set under the filter in one go.
    View {
        mask: u8,
    },
    /// A view frame as `replication.rs` runs it: asked under the filter (`Ask`), checked without
    /// it (`Run`), and its answer set under the filter again (`Land`, which runs it first if
    /// `Run` didn't); windows may be delivered in between. A new `Ask` drops an open one.
    Ask {
        mask: u8,
    },
    Run,
    Land,
    /// The pending changes as one window: `bulk` the plugin's keyless `c` (keys lost), `starved`
    /// every check slot held by another filter for the whole delivery.
    Deliver {
        bulk: bool,
        starved: bool,
    },
}

const OBJECTS: usize = 4;

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0..OBJECTS, any::<bool>(), any::<bool>()).prop_map(|(i, in_a, hidden)| Step::Put { i, in_a, hidden }),
        1 => (0..OBJECTS).prop_map(|i| Step::Delete { i }),
        1 => any::<u8>().prop_map(|mask| Step::View { mask }),
        1 => any::<u8>().prop_map(|mask| Step::Ask { mask }),
        1 => Just(Step::Run),
        1 => Just(Step::Land),
        3 => (any::<bool>(), any::<bool>()).prop_map(|(bulk, starved)| Step::Deliver { bulk, starved }),
    ]
}

/// One object's truth: `None` absent, else (in bucket A, hidden from the role).
type Truth = Option<(bool, bool)>;

fn shown_to_role(t: Truth) -> bool {
    matches!(t, Some((true, false)))
}

/// An open view frame (`Step::Ask`).
struct OpenView {
    ask: realtime::filter::KeyCheck,
    keys: Vec<String>,
    /// Its answer, once run.
    seen: Option<Result<Vec<bool>, ()>>,
    /// Keys a delivered window's check could have changed since it ran (`all`: a bulk window).
    stale: HashSet<String>,
    all: bool,
}

async fn run_storage_model(steps: Vec<Step>) -> Result<(), TestCaseError> {
    let ids: Vec<String> = (0..OBJECTS).map(|i| oid(200 + i)).collect();
    let limiter = Limiter::new(1);
    let checker = FakeChecker::new();
    checker.set_bypass(Some("authenticated"), false);
    let mut filter = TableFilter::new(
        target(),
        objects(),
        checker.clone(),
        limiter.clone(),
        VisibilityCache::new(Duration::from_millis(100)),
        // One key per check and a carry of two keys: chunks, and a carry that overflows.
        FilterSettings {
            max_keys: 1,
            carry_windows: 2,
            ..FilterSettings::default()
        },
    );
    filter.set_bucket(Some(BUCKET_A));
    let (tx, mut rx) = mpsc::channel(1024);
    filter.add(2, role("authenticated"), false, tx);

    let mut truth: Vec<Truth> = vec![None; OBJECTS];
    // Truth when the open window started.
    let mut before: Vec<Truth> = truth.clone();
    let mut touched: Vec<usize> = vec![];
    // Since the last delivery the filter could check: touched objects, and whether a bulk
    // window (keys lost) came. Some may have been trimmed from the carry instead of checked.
    let mut unchecked: Vec<usize> = vec![];
    let mut bulk_unchecked = false;
    // Keys the viewer was told exist (an insert or update frame, or a view confirmed after
    // every checked window that could have changed it) and not told gone since.
    let mut shown: HashSet<String> = HashSet::new();
    // Keys found gone from the viewer since (checked, or trimmed unchecked): no delete may
    // follow until shown again.
    let mut gone: HashSet<String> = HashSet::new();
    let mut open: Option<OpenView> = None;

    for step in steps {
        match step {
            Step::Put { i, in_a, hidden } => {
                let k = ids[i].as_str();
                checker.insert_rows(REL, &[k]);
                checker.place(&[k], if in_a { BUCKET_A } else { BUCKET_B });
                if hidden {
                    checker.hide(Some("authenticated"), &[k]);
                } else {
                    checker.unhide(Some("authenticated"), &[k]);
                }
                truth[i] = Some((in_a, hidden));
                if !touched.contains(&i) {
                    touched.push(i);
                }
            }
            Step::Delete { i } => {
                if truth[i].is_none() {
                    continue;
                }
                checker.delete_rows(REL, &[ids[i].as_str()]);
                truth[i] = None;
                if !touched.contains(&i) {
                    touched.push(i);
                }
            }
            Step::View { mask } => {
                let keys: Vec<String> = (0..OBJECTS)
                    .filter(|i| mask & (1 << i) != 0)
                    .map(|i| ks(&ids[i]))
                    .collect();
                filter.view(2, keys.clone()).await;
                for (i, k) in (0..OBJECTS).filter(|i| mask & (1 << i) != 0).zip(keys) {
                    if shown_to_role(truth[i]) {
                        gone.remove(&k);
                        shown.insert(k);
                    }
                }
            }
            Step::Ask { mask } => {
                let keys: Vec<String> = (0..OBJECTS)
                    .filter(|i| mask & (1 << i) != 0)
                    .map(|i| ks(&ids[i]))
                    .collect();
                let ask = filter.ask_keys(2, &keys, false).unwrap();
                open = Some(OpenView {
                    ask,
                    keys,
                    seen: None,
                    stale: HashSet::new(),
                    all: false,
                });
            }
            Step::Run => {
                if let Some(o) = open.as_mut().filter(|o| o.seen.is_none()) {
                    o.seen = Some(o.ask.run().await);
                }
            }
            Step::Land => {
                let Some(mut o) = open.take() else {
                    continue;
                };
                let seen = match o.seen.take() {
                    Some(seen) => seen,
                    None => o.ask.run().await,
                };
                // Confirmed by an answer no checked window has overtaken (`stale` stays empty
                // when it ran just now): the viewer may hold these as on its screen.
                if let Ok(answers) = &seen {
                    for (k, visible) in o.keys.iter().zip(answers) {
                        if *visible && !o.all && !o.stale.contains(k) {
                            gone.remove(k);
                            shown.insert(k.clone());
                        }
                    }
                }
                filter.set_view(2, &o.ask, o.keys.clone(), seen);
            }
            Step::Deliver { bulk, starved } => {
                let mut w = RelWindow {
                    rel: REL,
                    changed: bulk,
                    ..RelWindow::default()
                };
                if !bulk {
                    for &i in &touched {
                        let t = vec![ids[i].clone()];
                        match (before[i].is_some(), truth[i].is_some()) {
                            (false, true) => w.inserts.push(t),
                            (true, true) => w.updates.push(t),
                            _ => w.deletes.push(t),
                        }
                    }
                }
                unchecked.append(&mut touched);
                bulk_unchecked |= bulk;
                before = truth.clone();
                let held = starved.then(|| limiter.try_permit().unwrap());
                filter.deliver(&w, &[10], Instant::now()).await;
                drop(held);
                let events = drain(&mut rx);
                if starved {
                    prop_assert_eq!(
                        events,
                        vec![],
                        "a starved delivery carries, it sends nothing"
                    );
                    continue;
                }
                let visible = |k: &str| {
                    (0..OBJECTS).any(|i| ids[i] == k[2..k.len() - 2] && shown_to_role(truth[i]))
                };
                for e in &events {
                    let ChannelEvent::Rows(changes) = e else {
                        return Err(TestCaseError::fail(format!("not rows: {e:?}")));
                    };
                    for c in changes {
                        let Some(keys) = &c.keys else {
                            return Err(TestCaseError::fail(format!("keyless: {c:?}")));
                        };
                        for k in keys {
                            match c.op {
                                ChangeOp::Insert | ChangeOp::Update => {
                                    prop_assert!(
                                        visible(k),
                                        "{:?} of {} the role can't list in bucket A",
                                        c.op,
                                        k
                                    );
                                    gone.remove(k);
                                    shown.insert(k.clone());
                                }
                                ChangeOp::Delete => {
                                    prop_assert!(!visible(k), "a delete of {} it still sees", k);
                                    prop_assert!(
                                        shown.contains(k),
                                        "a delete of {} it was never shown",
                                        k
                                    );
                                    prop_assert!(
                                        !gone.contains(k),
                                        "a delete of {} checked gone before",
                                        k
                                    );
                                    shown.remove(k);
                                }
                                other => return Err(TestCaseError::fail(format!("{other:?}"))),
                            }
                        }
                    }
                }
                // Everything carried so far was checked now, or trimmed from the carry: what the
                // viewer was shown and no longer sees is gone (a bulk window re-checks everything
                // it was shown).
                let checked: Vec<String> = if bulk_unchecked {
                    shown.iter().cloned().collect()
                } else {
                    unchecked.iter().map(|&i| ks(&ids[i])).collect()
                };
                for k in &checked {
                    if shown.contains(k) && !visible(k) {
                        shown.remove(k);
                        gone.insert(k.clone());
                    }
                }
                // An open view's answer no longer speaks for what these windows changed.
                if let Some(o) = open.as_mut().filter(|o| o.seen.is_some()) {
                    o.all |= bulk_unchecked;
                    o.stale.extend(unchecked.iter().map(|&i| ks(&ids[i])));
                }
                unchecked.clear();
                bulk_unchecked = false;
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: std::env::var("STORAGE_MODEL_CASES").ok().and_then(|c| c.parse().ok()).unwrap_or(512),
        ..ProptestConfig::default()
    })]

    /// The invariant over interleavings: frames only for keys checked visible to the viewer in
    /// its bucket (inserts, updates), keyed deletes only for keys it was shown and that weren't
    /// found gone since, nothing keyless, nothing while starved.
    #[test]
    fn storage_frames_derive_only_from_keys_checked_visible_in_the_bucket(
        steps in proptest::collection::vec(step(), 1..48),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(run_storage_model(steps))?;
    }
}

// --- the socket (AC 4) -----------------------------------------------------------------------

#[tokio::test]
async fn storage_join_is_authorized_like_the_console() {
    let h = common::Harness::start().await;
    let mut ws = h.authed("ana").await;
    ws.send(json!({"type": "join", "ref": "1", "channel": "storage", "params": storage_params(json!("authenticated"))}))
        .await;
    let joined = ws.reply("1").await;
    assert_eq!(joined["type"], "joined", "{joined}");
    let want = format!(
        r#"storage:["{}","{}","app","{BUCKET_A}"]"#,
        common::DB,
        common::BR
    );
    assert_eq!(joined["channel"], want.as_str());
    let calls = h.orch.authorize_calls();
    assert_eq!(
        calls.last().unwrap()["target"],
        json!({"database_id": common::DB, "branch_id": common::BR, "pg_database": "app", "role": "authenticated"}),
        "the target the console's routes authorize"
    );
    assert_eq!(h.source.watchers("objects"), 1);
    // No presence state on a storage channel.
    ws.send(json!({"type": "presence", "channel": want, "state": {"row": null, "column": null, "editing": false}}))
        .await;
    let e = ws.frame_of("error").await;
    assert_eq!(e["code"], "invalid", "{e}");
}

#[tokio::test]
async fn storage_join_refused_by_the_orchestrator_is_forbidden_and_revocation_leaves() {
    let h = common::Harness::start().await;
    h.orch
        .on_authorize(|body| match body["target"]["role"].as_str() {
            Some("forbidden_role") => common::refused("forbidden"),
            _ => common::allowed("ana"),
        });
    let mut ws = h.authed("ana").await;
    ws.send(json!({"type": "join", "ref": "1", "channel": "storage", "params": storage_params(json!("forbidden_role"))}))
        .await;
    let e = ws.reply("1").await;
    assert_eq!(
        (e["type"].as_str(), e["code"].as_str()),
        (Some("error"), Some("forbidden")),
        "{e}"
    );

    ws.send(json!({"type": "join", "ref": "2", "channel": "storage", "params": storage_params(json!("authenticated"))}))
        .await;
    let joined = ws.reply("2").await;
    assert_eq!(joined["type"], "joined", "{joined}");
    h.orch.on_authorize(|_| common::refused("forbidden"));
    h.publish(
        &format!("chelabase.rt.access.{}", common::TENANT),
        &json!({"tenant_id": common::TENANT, "member_sub": "ana"}),
    )
    .await;
    let e = ws.frame_of("error").await;
    assert_eq!(
        (e["code"].as_str(), e["channel"].as_str()),
        (Some("forbidden"), joined["channel"].as_str()),
        "{e}"
    );
}
