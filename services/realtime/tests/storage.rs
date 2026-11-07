//! Object storage m17: the `storage:` channel (Studio's object browser). Parsing and the
//! `table:` refusal of the `storage` schema (AC 3), the bucket filter (AC 5), the per-viewer
//! read check dropping ids the viewer's role can't read (AC 2), and the socket's join,
//! authorization and revocation of a storage channel (AC 4).

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod common;
mod fakes;

use std::sync::Arc;
use std::time::Duration;

use chelabase_query::realtime::Relation;
use fakes::*;
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
