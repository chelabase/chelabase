//! RT1 T8: the merge window (spec §3.3, §8.1: 100 ms, more than 1000 keys → `changed`), schema
//! changes in the stream (R2, W10: by OID, `all` included) and partition fan-out (R16, W25).

use std::collections::HashMap;
use std::time::Duration;

use realtime::decoder::{Body, Op, RelChange, Txn};
use realtime::merge::{MAX_KEYS, Merger, RelWindow, WINDOW, Window};
use realtime::partition::{JoinKind, Parent, fan_out, join_kind};
use realtime::schema::is_schema_change;
use tokio::time::Instant;
use tokio_postgres::types::PgLsn;

fn k(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|s| s.to_string()).collect()
}

fn ch(rel: u32, op: Op, keys: &[&[&str]]) -> RelChange {
    RelChange {
        rel,
        schema: "public".into(),
        table: format!("t{rel}"),
        op,
        keys: keys.iter().map(|r| k(r)).collect(),
    }
}

fn txn(lsn: u64, xid: u64, changes: Vec<RelChange>) -> Txn {
    Txn {
        lsn: Some(PgLsn::from(lsn)),
        xid: Some(xid),
        body: Body::Changes(changes),
    }
}

fn merged(txns: &[Txn]) -> Window {
    let mut m = Merger::new(MAX_KEYS);
    let t0 = Instant::now();
    for t in txns {
        m.push(t0, t);
    }
    let w = m.take();
    assert!(w.is_some(), "the merger holds the open window");
    w.unwrap()
}

/// A window built by hand (schema and partition tests don't depend on the merger).
fn win(rels: Vec<RelWindow>, all: bool) -> Window {
    Window {
        all,
        rels,
        xids: vec![1],
        end_lsn: Some(PgLsn::from(1)),
        split: vec![],
    }
}

fn rw(rel: u32, inserts: &[&[&str]], deletes: &[&[&str]], changed: bool) -> RelWindow {
    RelWindow {
        rel,
        changed,
        inserts: inserts.iter().map(|r| k(r)).collect(),
        updates: vec![],
        deletes: deletes.iter().map(|r| k(r)).collect(),
    }
}

fn only(w: &Window, rel: u32) -> RelWindow {
    w.rel(rel)
        .unwrap_or_else(|| panic!("relation {rel} in {w:?}"))
        .clone()
}

#[test]
fn the_window_closes_100_ms_after_its_first_transaction() {
    let mut m = Merger::new(MAX_KEYS);
    let t0 = Instant::now();
    m.push(t0, &txn(0x10, 1, vec![ch(1, Op::Insert, &[&["1"]])]));
    assert_eq!(m.due_at(), Some(t0 + Duration::from_millis(100)));
    assert_eq!(WINDOW, Duration::from_millis(100));
    assert!(!m.is_empty());
    m.push(
        t0 + Duration::from_millis(60),
        &txn(0x20, 2, vec![ch(1, Op::Insert, &[&["2"]])]),
    );
    assert_eq!(
        m.due_at(),
        Some(t0 + WINDOW),
        "a later transaction doesn't extend the window"
    );
    let w = m.take().unwrap();
    assert_eq!(only(&w, 1).inserts, vec![k(&["1"]), k(&["2"])]);
    assert_eq!(w.xids, vec![1, 2]);
    assert_eq!(w.end_lsn, Some(PgLsn::from(0x20)));
    assert!(m.is_empty() && m.due_at().is_none() && m.take().is_none());
    let t1 = t0 + Duration::from_millis(500);
    m.push(t1, &txn(0x30, 3, vec![ch(1, Op::Delete, &[&["1"]])]));
    assert_eq!(m.due_at(), Some(t1 + WINDOW), "the next window opens fresh");
}

#[test]
fn each_key_once_with_one_op() {
    let w = merged(&[
        // an insert then an update in later transactions → still an insert
        txn(1, 1, vec![ch(1, Op::Insert, &[&["a"]])]),
        txn(2, 2, vec![ch(1, Op::Update, &[&["a"]])]),
        // an insert then a delete → a delete
        txn(3, 3, vec![ch(1, Op::Insert, &[&["b"]])]),
        txn(4, 4, vec![ch(1, Op::Delete, &[&["b"]])]),
        // a delete then an insert → an update (the row exists, maybe with new values)
        txn(5, 5, vec![ch(1, Op::Delete, &[&["c"]])]),
        txn(6, 6, vec![ch(1, Op::Insert, &[&["c"]])]),
        // updated twice → one update
        txn(7, 7, vec![ch(1, Op::Update, &[&["d"]])]),
        txn(8, 8, vec![ch(1, Op::Update, &[&["d"]])]),
        // an update then a delete → a delete
        txn(9, 9, vec![ch(1, Op::Update, &[&["e"]])]),
        txn(10, 10, vec![ch(1, Op::Delete, &[&["e"]])]),
    ]);
    let r = only(&w, 1);
    assert_eq!(r.inserts, vec![k(&["a"])]);
    assert_eq!(r.updates, vec![k(&["c"]), k(&["d"])]);
    assert_eq!(r.deletes, vec![k(&["b"]), k(&["e"])]);
    assert!(!r.changed);
}

#[test]
fn a_key_under_insert_and_delete_of_one_transaction_is_re_read() {
    // The plugin groups per op (i, u, d), so the order inside a transaction is lost: the row may
    // exist (deleted and inserted again) or not (inserted and deleted). An update makes the
    // client re-read it by key (and fade it if it's gone).
    let w = merged(&[txn(
        1,
        1,
        vec![
            ch(1, Op::Insert, &[&["1"], &["2"]]),
            ch(1, Op::Update, &[&["1"]]),
            ch(1, Op::Delete, &[&["2"]]),
        ],
    )]);
    let r = only(&w, 1);
    assert_eq!(r.inserts, vec![k(&["1"])], "i + u in one transaction: new");
    assert_eq!(
        r.updates,
        vec![k(&["2"])],
        "i + d in one transaction: re-read"
    );
    assert!(r.deletes.is_empty(), "{r:?}");
}

#[test]
fn a_pk_update_is_a_delete_and_an_insert() {
    let w = merged(&[txn(
        1,
        1,
        vec![ch(1, Op::Insert, &[&["2"]]), ch(1, Op::Delete, &[&["1"]])],
    )]);
    let r = only(&w, 1);
    assert_eq!((r.inserts, r.deletes), (vec![k(&["2"])], vec![k(&["1"])]));
}

#[test]
fn more_than_1000_keys_in_a_window_is_changed() {
    let first: Vec<Vec<String>> = (0..600).map(|i| vec![i.to_string()]).collect();
    let second: Vec<Vec<String>> = (600..1001).map(|i| vec![i.to_string()]).collect();
    let mut m = Merger::new(MAX_KEYS);
    let t0 = Instant::now();
    let rel_change = |keys: Vec<Vec<String>>| RelChange {
        rel: 1,
        schema: "public".into(),
        table: "t".into(),
        op: Op::Insert,
        keys,
    };
    m.push(t0, &txn(1, 1, vec![rel_change(first.clone())]));
    m.push(t0, &txn(2, 2, vec![rel_change(second[..400].to_vec())]));
    let w = m.take();
    assert!(w.is_some(), "the merger holds the open window");
    let w = w.unwrap();
    let r = only(&w, 1);
    assert!(!r.changed, "exactly 1000 keys stay keyed");
    assert_eq!(r.key_count(), 1000);

    m.push(t0, &txn(3, 3, vec![rel_change(first)]));
    m.push(t0, &txn(4, 4, vec![rel_change(second)]));
    let w = m.take().unwrap();
    let r = only(&w, 1);
    assert!(r.changed, "1001 distinct keys → changed");
    assert_eq!(r.key_count(), 0, "a changed relation carries no keys");
}

#[test]
fn the_limit_counts_distinct_keys_over_every_op() {
    let ins: Vec<Vec<String>> = (0..800).map(|i| vec![i.to_string()]).collect();
    let del: Vec<Vec<String>> = (800..1001).map(|i| vec![i.to_string()]).collect();
    let w = merged(&[txn(
        1,
        1,
        vec![
            RelChange {
                rel: 1,
                schema: "public".into(),
                table: "t".into(),
                op: Op::Insert,
                keys: ins,
            },
            RelChange {
                rel: 1,
                schema: "public".into(),
                table: "t".into(),
                op: Op::Delete,
                keys: del,
            },
        ],
    )]);
    assert!(only(&w, 1).changed);
}

#[test]
fn a_c_change_makes_the_relation_changed_and_drops_its_keys() {
    let w = merged(&[
        txn(
            1,
            1,
            vec![ch(1, Op::Insert, &[&["1"]]), ch(2, Op::Insert, &[&["x"]])],
        ),
        txn(2, 2, vec![ch(1, Op::Changed, &[])]),
        txn(3, 3, vec![ch(1, Op::Update, &[&["2"]])]),
    ]);
    let r = only(&w, 1);
    assert!(r.changed);
    assert_eq!(r.key_count(), 0, "{r:?}");
    let other = only(&w, 2);
    assert!(!other.changed);
    assert_eq!(other.inserts, vec![k(&["x"])]);
    assert!(!w.all);
}

#[test]
fn an_all_transaction_makes_the_window_all() {
    let w = merged(&[
        txn(1, 1, vec![ch(1, Op::Insert, &[&["1"]])]),
        Txn {
            lsn: Some(PgLsn::from(2)),
            xid: Some(2),
            body: Body::All,
        },
    ]);
    assert!(w.all);
    assert_eq!(w.xids, vec![1, 2]);
    assert_eq!(w.end_lsn, Some(PgLsn::from(2)));
}

#[test]
fn relations_keep_first_change_order() {
    let w = merged(&[
        txn(1, 1, vec![ch(3, Op::Insert, &[&["1"]])]),
        txn(
            2,
            2,
            vec![ch(1, Op::Insert, &[&["1"]]), ch(3, Op::Insert, &[&["2"]])],
        ),
    ]);
    let order: Vec<u32> = w.rels.iter().map(|r| r.rel).collect();
    assert_eq!(order, vec![3, 1]);
}

// --- schema changes (R2, W10) ---

const VERSION_OID: u32 = 16_390;

#[test]
fn a_change_of_schema_version_by_oid_is_a_schema_change() {
    let w = win(
        vec![
            rw(1, &[&["1"]], &[], false),
            rw(VERSION_OID, &[], &[], false),
        ],
        false,
    );
    assert!(is_schema_change(&w, Some(VERSION_OID)));
    assert!(!is_schema_change(
        &win(vec![rw(1, &[&["1"]], &[], false)], false),
        Some(VERSION_OID)
    ));
}

#[test]
fn the_names_in_a_message_never_decide() {
    // The window only knows OIDs: a customer table named `chelabase.schema_version` in another
    // schema has another OID and is no schema change; the version table is recognised by OID.
    let version = win(vec![rw(VERSION_OID, &[], &[], true)], false);
    assert!(is_schema_change(&version, Some(VERSION_OID)), "by OID");
    let lookalike = win(vec![rw(20_000, &[&["t"]], &[], false)], false);
    assert!(!is_schema_change(&lookalike, Some(VERSION_OID)));
    assert!(
        !is_schema_change(&version, None),
        "a database without the data API has no version table"
    );
}

#[test]
fn an_all_window_is_a_schema_change() {
    let all = win(vec![], true);
    assert!(is_schema_change(&all, Some(VERSION_OID)));
    assert!(
        is_schema_change(&all, None),
        "all hides what changed, wherever"
    );
}

#[test]
fn the_merger_feeds_the_schema_check() {
    let w = merged(&[txn(
        1,
        1,
        vec![
            ch(1, Op::Insert, &[&["1"]]),
            RelChange {
                rel: VERSION_OID,
                schema: "chelabase".into(),
                table: "schema_version".into(),
                op: Op::Update,
                keys: vec![k(&["t"])],
            },
        ],
    )]);
    assert!(is_schema_change(&w, Some(VERSION_OID)));
}

// --- partitions (R16, W25) ---

fn by_rel(out: Vec<RelWindow>) -> HashMap<u32, RelWindow> {
    out.into_iter().map(|r| (r.rel, r)).collect()
}

fn keyed(oid: u32) -> Parent {
    Parent { oid, keyed: true }
}

#[test]
fn a_leaf_change_reaches_the_leaf_its_parent_and_the_root() {
    // root 100 ← mid 101 ← leaf 102
    let parents = HashMap::from([(102, vec![keyed(101), keyed(100)]), (101, vec![keyed(100)])]);
    let w = win(vec![rw(102, &[&["1"]], &[&["2"]], false)], false);
    let by = by_rel(fan_out(&w, &parents, MAX_KEYS));
    for rel in [100, 101, 102] {
        let r = by
            .get(&rel)
            .unwrap_or_else(|| panic!("relation {rel} reached: {by:?}"));
        assert_eq!(r.inserts, vec![k(&["1"])], "{rel}");
        assert_eq!(r.deletes, vec![k(&["2"])], "{rel}");
        assert!(!r.changed, "{rel}");
    }
    assert_eq!(by.len(), 3);
}

#[test]
fn leaves_of_one_root_merge_and_the_limit_applies_after_the_union() {
    let parents = HashMap::from([(11, vec![keyed(10)]), (12, vec![keyed(10)])]);
    let w = win(
        vec![rw(11, &[&["1"]], &[], false), rw(12, &[&["2"]], &[], false)],
        false,
    );
    let by = by_rel(fan_out(&w, &parents, MAX_KEYS));
    assert_eq!(
        by.get(&10).map(|r| r.inserts.clone()),
        Some(vec![k(&["1"]), k(&["2"])])
    );
    assert_eq!(by[&11].inserts, vec![k(&["1"])]);

    let by = by_rel(fan_out(&w, &parents, 1));
    assert!(by[&10].changed, "two keys in the root over a limit of 1");
    assert_eq!(by[&10].key_count(), 0);
    assert!(!by[&11].changed && !by[&12].changed);
}

#[test]
fn an_inheritance_child_makes_its_parent_changed() {
    let parents = HashMap::from([(
        21,
        vec![Parent {
            oid: 20,
            keyed: false,
        }],
    )]);
    let w = win(vec![rw(21, &[&["1"]], &[], false)], false);
    let by = by_rel(fan_out(&w, &parents, MAX_KEYS));
    assert_eq!(
        by.get(&20).map(|r| (r.changed, r.key_count())),
        Some((true, 0)),
        "the parent reads its children: changed"
    );
    assert_eq!(by[&21].inserts, vec![k(&["1"])], "the child keeps its keys");
}

#[test]
fn a_leafs_c_is_its_ancestors_changed() {
    let parents = HashMap::from([(31, vec![keyed(30)])]);
    let w = win(vec![rw(31, &[], &[], true)], false);
    let by = by_rel(fan_out(&w, &parents, MAX_KEYS));
    assert_eq!(by.get(&30).map(|r| r.changed), Some(true));
    assert!(by[&31].changed);
}

#[test]
fn a_relation_without_parents_is_passed_through() {
    let r = rw(5, &[&["1"]], &[&["2"]], false);
    let out = fan_out(&win(vec![r.clone()], false), &HashMap::new(), MAX_KEYS);
    assert_eq!(out, vec![r]);
}

#[test]
fn relation_kinds() {
    assert_eq!(join_kind('r', false), JoinKind::Keys, "a table");
    assert_eq!(join_kind('p', false), JoinKind::Keys, "a partitioned table");
    assert_eq!(
        join_kind('r', true),
        JoinKind::Changed,
        "a legacy inheritance parent reads its children's rows"
    );
    for kind in ['v', 'm', 'f', 'S', 'c', 'i', 't'] {
        assert_eq!(join_kind(kind, false), JoinKind::NotATable, "{kind}");
    }
}

// --- object storage m17: `storage.objects` is never collapsed by the window ---

fn objects_ch(rel: u32, op: Op, keys: &[&[&str]]) -> RelChange {
    RelChange {
        schema: "storage".into(),
        table: "objects".into(),
        ..ch(rel, op, keys)
    }
}

/// Storage channels must never get a keyless `changed` for another bucket's changes, so
/// `storage.objects` keeps every key of a window past the limit (they're checked in chunks), and
/// a `c` in the window keeps the other transactions' keys.
#[test]
fn storage_objects_keeps_every_key_past_the_limit() {
    let mut m = Merger::new(2);
    let t0 = Instant::now();
    m.push(
        t0,
        &txn(0x10, 1, vec![objects_ch(7, Op::Insert, &[&["a"], &["b"]])]),
    );
    m.push(
        t0,
        &txn(0x20, 2, vec![objects_ch(7, Op::Delete, &[&["c"], &["d"]])]),
    );
    m.push(
        t0,
        &txn(0x30, 3, vec![ch(8, Op::Insert, &[&["1"], &["2"], &["3"]])]),
    );
    let w = m.take().unwrap();
    let objects = only(&w, 7);
    assert!(!objects.changed, "{objects:?}");
    assert_eq!(objects.inserts, vec![k(&["a"]), k(&["b"])]);
    assert_eq!(objects.deletes, vec![k(&["c"]), k(&["d"])]);
    assert!(only(&w, 8).changed, "another table keeps the limit");
    assert_eq!(w.split, vec![7], "only storage.objects is kept whole");

    m.push(
        t0,
        &txn(0x40, 4, vec![objects_ch(7, Op::Insert, &[&["e"]])]),
    );
    m.push(t0, &txn(0x50, 5, vec![objects_ch(7, Op::Changed, &[])]));
    m.push(
        t0,
        &txn(0x60, 6, vec![objects_ch(7, Op::Update, &[&["f"]])]),
    );
    let objects = only(&m.take().unwrap(), 7);
    assert!(objects.changed, "the `c` is kept");
    assert_eq!(
        objects.inserts,
        vec![k(&["e"])],
        "and so are the other keys"
    );
    assert_eq!(objects.updates, vec![k(&["f"])]);
}

#[test]
fn storage_objects_passes_the_fan_out_whole_and_is_published_in_chunks() {
    // Another relation's parents don't pull storage.objects into a union over the limit.
    let parents = HashMap::from([(11, vec![keyed(10)])]);
    let objects = rw(7, &[&["a"], &["b"], &["c"]], &[&["d"], &["e"]], false);
    let mut w = win(vec![objects.clone(), rw(11, &[&["1"]], &[], false)], false);
    w.split = vec![7];
    let out = fan_out(&w, &parents, 2);
    assert_eq!(
        out.iter().filter(|r| r.rel == 7).collect::<Vec<_>>(),
        vec![&objects]
    );
    assert_eq!(by_rel(out)[&10].inserts, vec![k(&["1"])]);

    let chunks = RelWindow {
        changed: true,
        ..objects.clone()
    }
    .chunks(2);
    assert_eq!(chunks.len(), 3);
    assert!(chunks.iter().all(|c| c.key_count() <= 2 && c.rel == 7));
    assert_eq!(
        chunks.iter().filter(|c| c.changed).count(),
        1,
        "`changed` once"
    );
    let keys: Vec<Vec<String>> = chunks
        .iter()
        .flat_map(|c| {
            c.inserts
                .iter()
                .chain(&c.deletes)
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        keys,
        vec![k(&["a"]), k(&["b"]), k(&["c"]), k(&["d"]), k(&["e"])]
    );
    assert_eq!(
        objects.chunks(5),
        vec![objects.clone()],
        "a window that fits is one chunk"
    );
}
