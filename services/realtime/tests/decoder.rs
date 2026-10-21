//! RT1 T8: the `chelabase_keys` messages (pre-flight §2: every shape the plugin ships) and the
//! replication session's commands and startup settings (R1, W29, the fork's walsender rule).

use realtime::decoder::{
    self, Body, Op, RelChange, ReplicationTarget, Txn, create_slot_command, new_slot_name,
    replication_config, start_replication_command,
};
use tokio_postgres::config::ReplicationMode;
use tokio_postgres::types::PgLsn;

fn lsn(text: &str) -> PgLsn {
    text.parse().unwrap()
}

fn keys(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn change(rel: u32, t: &str, op: Op, k: &[&[&str]]) -> RelChange {
    RelChange {
        rel,
        schema: "public".into(),
        table: t.into(),
        op,
        keys: keys(k),
    }
}

#[test]
fn a_transaction_with_every_op_in_the_plugins_order() {
    // keys.out's first line: i 1,2; u 1; d 2 (a key under both i and d), then a `c`.
    let msg = br#"{"v":1,"lsn":"0/1A2B3C","xid":745,"changes":[{"rel":16412,"s":"public","t":"orders","op":"i","k":[["1"],["2"]]},{"rel":16412,"s":"public","t":"orders","op":"u","k":[["1"]]},{"rel":16412,"s":"public","t":"orders","op":"d","k":[["2"]]},{"rel":16500,"s":"public","t":"events","op":"c"}]}"#;
    let txn = decoder::parse(msg);
    assert_eq!(
        txn,
        Txn {
            lsn: Some(lsn("0/1A2B3C")),
            xid: Some(745),
            body: Body::Changes(vec![
                change(16412, "orders", Op::Insert, &[&["1"], &["2"]]),
                change(16412, "orders", Op::Update, &[&["1"]]),
                change(16412, "orders", Op::Delete, &[&["2"]]),
                change(16500, "events", Op::Changed, &[]),
            ]),
        }
    );
}

#[test]
fn the_xid_is_a_full_xid8_and_the_lsn_the_commits_end() {
    // Past 2^32: an epoch-qualified xid compares with pg_current_xact_id(), never wraps.
    let msg = br#"{"v":1,"lsn":"1F/FFFF0010","xid":4294967301,"changes":[{"rel":1,"s":"s","t":"t","op":"i","k":[["9"]]}]}"#;
    let txn = decoder::parse(msg);
    assert_eq!(txn.xid, Some(4_294_967_301));
    assert_eq!(txn.lsn, Some(lsn("1F/FFFF0010")));
    assert_eq!(u64::from(txn.lsn.unwrap()), 0x1F_FFFF_0010);
}

#[test]
fn composite_keys_keep_pk_order_and_exact_texts() {
    let msg = br#"{"v":1,"lsn":"0/10","xid":3,"changes":[{"rel":7,"s":"app","t":"line items","op":"i","k":[["9007199254740993","1.50","2026-01-02 03:04:05+00"],["-0","he said \"hi\"","a\\b"]]}]}"#;
    let txn = decoder::parse(msg);
    let Body::Changes(changes) = txn.body else {
        panic!("keys expected: {txn:?}");
    };
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].schema, "app");
    assert_eq!(changes[0].table, "line items");
    assert_eq!(
        changes[0].keys,
        keys(&[
            &["9007199254740993", "1.50", "2026-01-02 03:04:05+00"],
            &["-0", "he said \"hi\"", "a\\b"],
        ]),
        "texts are never re-parsed as numbers"
    );
}

#[test]
fn an_all_transaction() {
    let txn = decoder::parse(br#"{"v":1,"lsn":"0/2000","xid":9,"all":true}"#);
    assert_eq!(
        txn,
        Txn {
            lsn: Some(lsn("0/2000")),
            xid: Some(9),
            body: Body::All
        }
    );
}

#[test]
fn unknown_fields_are_ignored() {
    let txn = decoder::parse(
        br#"{"v":1,"lsn":"0/30","xid":4,"origin":"x","ts":"2026","changes":[{"rel":5,"s":"public","t":"t","op":"i","k":[["1"]],"extra":{"a":1}}],"next":[1,2]}"#,
    );
    assert_eq!(txn.lsn, Some(lsn("0/30")));
    assert_eq!(
        txn.body,
        Body::Changes(vec![change(5, "t", Op::Insert, &[&["1"]])])
    );
}

#[test]
fn another_version_is_everything_changed() {
    for msg in [
        &br#"{"v":2,"lsn":"0/40","xid":5,"changes":[{"rel":5,"s":"public","t":"t","op":"i","k":[["1"]]}]}"#[..],
        br#"{"lsn":"0/40","xid":5,"changes":[]}"#,
        br#"{"v":"1","lsn":"0/40","xid":5,"changes":[]}"#,
    ] {
        let txn = decoder::parse(msg);
        assert_eq!(
            txn.body,
            Body::All,
            "{}: never keys from a message this build doesn't know",
            String::from_utf8_lossy(msg)
        );
    }
}

#[test]
fn an_unreadable_message_fails_closed() {
    for msg in [
        &b"not json"[..],
        b"",
        br#"{"v":1,"lsn":"0/50","xid":6,"changes":[{"rel":5,"s":"public","t":"t","op":"x","k":[["1"]]}]}"#,
        br#"{"v":1,"lsn":"0/50","xid":6,"changes":[{"rel":5,"s":"public","t":"t","op":"i","k":[[1]]}]}"#,
        br#"{"v":1,"lsn":"0/50","xid":6,"changes":[{"rel":5,"s":"public","t":"t","op":"i"}]}"#,
        br#"{"v":1,"lsn":"zz","xid":6,"changes":[]}"#,
    ] {
        let txn = decoder::parse(msg);
        assert_eq!(
            txn.body,
            Body::All,
            "{}: an unknown op, a non-string key text, a keyed op without keys or a bad LSN is `changed` for everything",
            String::from_utf8_lossy(msg)
        );
    }
}

#[test]
fn the_slot_is_temporary_logical_chelabase_keys_without_an_exported_snapshot() {
    let slot = new_slot_name();
    assert!(slot.starts_with("rt_"), "{slot}");
    assert!(slot.len() > 3 && slot.len() <= 63, "{slot}");
    assert!(
        slot.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
        "{slot}"
    );
    assert_ne!(new_slot_name(), slot, "random per session");
    assert_eq!(
        create_slot_command(&slot),
        format!(
            "CREATE_REPLICATION_SLOT {slot} TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing')"
        )
    );
}

#[test]
fn start_replication_sends_the_three_options() {
    assert_eq!(
        start_replication_command("rt_abc", lsn("16/B374D848")),
        r#"START_REPLICATION SLOT rt_abc LOGICAL 16/B374D848 ("max_keys_per_table" '1000', "max_tables" '256', "max_message_bytes" '1048576')"#
    );
}

#[test]
fn the_startup_packet_carries_only_what_the_fork_allows() {
    let config = replication_config(&ReplicationTarget {
        host: "10.1.2.3".into(),
        port: 6543,
        server_name: Some("ep-1.compute.chelabase.internal".into()),
        database: "app db".into(),
        password: "secret".into(),
    });
    assert_eq!(config.get_user(), Some("chelabase_realtime"));
    assert_eq!(config.get_dbname(), Some("app db"));
    assert_eq!(
        config.get_replication_mode(),
        Some(ReplicationMode::Logical)
    );
    assert_eq!(config.get_application_name(), Some("chelabase_realtime"));
    assert_eq!(
        config.get_options(),
        None,
        "any `options` content is FATAL for the role"
    );
    assert_eq!(config.get_password(), Some(&b"secret"[..]));
    assert_eq!(config.get_ports(), &[6543]);
}
