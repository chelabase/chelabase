//! RT1 T7: presence diffing, instance expiry after three missed heartbeats, R4's scope drop,
//! the 1 KB state and 1000-member limits, 20 frames/s with the latest state winning (spec
//! §5.1, §5.2, R4, R15).

use std::time::{Duration, Instant};

use chelabase_nats::rt::PRESENCE;
use realtime::channel::Scope;
use realtime::presence::{
    ChannelPresence, Diff, MAX_STATE_BYTES, PresenceError, PresenceEvent, PresenceMsg,
    PresenceThrottle, blank, heartbeat_subject, parse_table_state, presence_subject, sync_subject,
};
use realtime::protocol::{Member, TableState};
use serde_json::json;
use uuid::Uuid;

fn scope() -> Scope {
    Scope::Table {
        branch_id: Uuid::from_u128(2),
        db_oid: 16384,
    }
}

const CH: &str = "table:[\"a\"]";

fn member(id: &str, row: Option<&str>) -> Member {
    Member {
        id: id.into(),
        sub: format!("sub-{id}"),
        name: Some("Ana".into()),
        email: None,
        role: None,
        color_seed: 1,
        state: TableState {
            row: row.map(str::to_string),
            column: Some("name".into()),
            editing: false,
        },
    }
}

fn msg(instance: &str, event: PresenceEvent) -> PresenceMsg {
    PresenceMsg {
        scope: scope(),
        channel: CH.into(),
        instance: instance.into(),
        event,
    }
}

fn join(instance: &str, m: Member) -> PresenceMsg {
    msg(instance, PresenceEvent::Join { member: m })
}

fn ids(ms: &[Member]) -> Vec<&str> {
    ms.iter().map(|m| m.id.as_str()).collect()
}

#[test]
fn subjects() {
    assert_eq!(presence_subject("abc"), format!("{PRESENCE}.abc"));
    assert_eq!(heartbeat_subject("i-1"), format!("{PRESENCE}.hb.i-1"));
    assert_eq!(sync_subject("abc"), format!("{PRESENCE}.sync.abc"));
    assert_eq!(PRESENCE, "chelabase.rt.presence");
}

#[test]
fn joins_updates_and_leaves() {
    let now = Instant::now();
    let mut p = ChannelPresence::new(scope(), CH, 1000);
    let d = p.apply(&join("i1", member("a", None)), now).unwrap();
    assert_eq!((ids(&d.joins), ids(&d.leaves)), (vec!["a"], vec![]));
    assert_eq!(
        p.apply(&join("i1", member("a", None)), now).unwrap(),
        Diff::default(),
        "the same state: no change"
    );
    let d = p
        .apply(&join("i1", member("a", Some("[\"1\"]"))), now)
        .unwrap();
    assert_eq!(
        d.leaves,
        vec![member("a", None)],
        "an update is a leave of the old state…"
    );
    assert_eq!(
        d.joins,
        vec![member("a", Some("[\"1\"]"))],
        "…and a join of the new one, same id"
    );
    p.apply(&join("i2", member("b", None)), now).unwrap();
    assert_eq!(
        ids(&p.members()),
        vec!["a", "b"],
        "sorted by id, across instances"
    );
    let d = p
        .apply(&msg("i1", PresenceEvent::Leave { id: "a".into() }), now)
        .unwrap();
    assert_eq!(d.leaves, vec![member("a", Some("[\"1\"]"))]);
    assert!(
        p.apply(&msg("i1", PresenceEvent::Leave { id: "zz".into() }), now)
            .unwrap()
            .is_empty()
    );
    assert_eq!(p.len(), 1);
}

#[test]
fn a_snapshot_replaces_that_instances_members() {
    let now = Instant::now();
    let mut p = ChannelPresence::new(scope(), CH, 1000);
    p.apply(&join("i1", member("a", None)), now).unwrap();
    p.apply(&join("i1", member("b", None)), now).unwrap();
    p.apply(&join("i2", member("c", None)), now).unwrap();
    let snap = msg(
        "i1",
        PresenceEvent::Snapshot {
            members: vec![member("b", Some("[\"9\"]")), member("d", None)],
        },
    );
    let d = p.apply(&snap, now).unwrap();
    let mut joins = ids(&d.joins);
    joins.sort();
    let mut leaves = ids(&d.leaves);
    leaves.sort();
    assert_eq!(joins, vec!["b", "d"], "b changed, d is new");
    assert_eq!(leaves, vec!["a", "b"], "a is gone, b's old state leaves");
    assert_eq!(
        ids(&p.members()),
        vec!["b", "c", "d"],
        "i2's members untouched"
    );
}

#[test]
fn three_missed_heartbeats_drop_an_instance() {
    let t0 = Instant::now();
    let after = Duration::from_secs(5) * 3;
    let mut p = ChannelPresence::new(scope(), CH, 1000);
    p.heartbeat("i1", t0);
    p.heartbeat("i2", t0);
    p.apply(&join("i1", member("a", None)), t0).unwrap();
    p.apply(&join("i2", member("b", None)), t0).unwrap();
    p.heartbeat("i2", t0 + Duration::from_secs(10));
    assert!(
        p.expire(t0 + Duration::from_millis(14_900), after)
            .is_empty(),
        "two missed: kept"
    );
    let d = p.expire(t0 + Duration::from_millis(15_100), after);
    assert_eq!(
        ids(&d.leaves),
        vec!["a"],
        "three missed: i1's members leave"
    );
    assert!(d.joins.is_empty());
    assert_eq!(ids(&p.members()), vec!["b"], "i2 heartbeat in time");
    assert!(
        p.expire(t0 + Duration::from_millis(15_200), after)
            .is_empty(),
        "once"
    );
}

#[test]
fn messages_from_another_scope_or_channel_are_dropped() {
    let now = Instant::now();
    let mut p = ChannelPresence::new(scope(), CH, 1000);
    // R4: the same channel string under another branch, database or project.
    for other in [
        Scope::Table {
            branch_id: Uuid::from_u128(3),
            db_oid: 16384,
        },
        Scope::Table {
            branch_id: Uuid::from_u128(2),
            db_oid: 16385,
        },
        Scope::Project {
            project_id: Uuid::from_u128(2),
        },
    ] {
        let mut m = join("i1", member("x", None));
        m.scope = other;
        assert_eq!(p.apply(&m, now), Err(PresenceError::ScopeMismatch));
    }
    let mut m = join("i1", member("x", None));
    m.channel = "table:[\"b\"]".into();
    assert_eq!(p.apply(&m, now), Err(PresenceError::ScopeMismatch));
    assert!(p.is_empty());
}

#[test]
fn a_channel_holds_at_most_its_maximum_of_members() {
    let now = Instant::now();
    let mut p = ChannelPresence::new(scope(), CH, 1000);
    for i in 0..1000 {
        p.apply(&join("i1", member(&format!("m{i:04}"), None)), now)
            .unwrap();
    }
    assert_eq!(
        p.apply(&join("i1", member("m1000", None)), now),
        Err(PresenceError::Full)
    );
    assert_eq!(p.len(), 1000);
    assert!(
        p.apply(&join("i1", member("m0001", Some("[\"1\"]"))), now)
            .is_ok(),
        "an update of a member is no new member"
    );
}

#[test]
fn table_state_is_checked() {
    assert_eq!(
        parse_table_state(&json!({"row": "[\"1\"]", "column": "name", "editing": true})),
        Ok(TableState {
            row: Some("[\"1\"]".into()),
            column: Some("name".into()),
            editing: true
        })
    );
    assert_eq!(
        parse_table_state(&json!({"row": null, "column": null, "editing": false})),
        Ok(TableState::default())
    );
    for bad in [
        json!({"row": ["1"], "column": null, "editing": false}),
        json!({"row": null, "column": 3, "editing": false}),
        json!({"row": null, "column": null, "editing": "yes"}),
        json!({"row": null, "column": null, "editing": false, "cursor": [1, 2]}),
        json!("x"),
        json!(null),
    ] {
        assert!(
            matches!(parse_table_state(&bad), Err(PresenceError::Invalid(_))),
            "{bad}"
        );
    }
}

#[test]
fn state_over_1_kb_is_refused() {
    assert_eq!(MAX_STATE_BYTES, 1024);
    let fill = |n: usize| json!({"row": "x".repeat(n), "column": null, "editing": false});
    let base = fill(0).to_string().len();
    assert!(
        parse_table_state(&fill(MAX_STATE_BYTES - base)).is_ok(),
        "exactly 1 KB"
    );
    assert_eq!(
        parse_table_state(&fill(MAX_STATE_BYTES - base + 1)),
        Err(PresenceError::TooLarge)
    );
}

#[test]
fn a_viewer_who_cant_see_the_row_gets_a_blank_state() {
    let m = member("a", Some("[\"1\"]"));
    let b = blank(&m);
    assert_eq!(b.state, TableState::default());
    assert_eq!(
        (b.id, b.sub, b.name, b.color_seed),
        (m.id, m.sub, m.name, m.color_seed),
        "the member stays visible"
    );
}

#[test]
fn twenty_frames_a_second_and_the_latest_wins() {
    let t0 = Instant::now();
    let state = |i: usize| TableState {
        row: Some(format!("[\"{i}\"]")),
        column: None,
        editing: false,
    };
    let mut t = PresenceThrottle::new(20);
    for i in 0..20 {
        assert_eq!(t.offer(state(i), t0), Some(state(i)), "frame {i} goes out");
    }
    for i in 20..25 {
        assert_eq!(
            t.offer(state(i), t0 + Duration::from_millis(10)),
            None,
            "frame {i} is held"
        );
    }
    assert_eq!(
        t.take_pending(t0 + Duration::from_millis(500)),
        None,
        "no slot yet"
    );
    assert_eq!(
        t.take_pending(t0 + Duration::from_secs(1)),
        Some(state(24)),
        "only the latest held state"
    );
    assert_eq!(t.take_pending(t0 + Duration::from_secs(2)), None);
}

#[test]
fn presence_messages_round_trip() {
    for event in [
        PresenceEvent::Join {
            member: member("a", Some("[\"1\"]")),
        },
        PresenceEvent::Leave { id: "a".into() },
        PresenceEvent::Snapshot {
            members: vec![member("a", None)],
        },
    ] {
        let m = msg("i1", event);
        let text = serde_json::to_string(&m).unwrap();
        assert_eq!(serde_json::from_str::<PresenceMsg>(&text).unwrap(), m);
    }
}
