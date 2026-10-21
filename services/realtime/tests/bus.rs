//! RT1 T7: the in-process bus the socket tests share between instances, and NATS subject
//! matching.

use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use realtime::bus::{Bus, MemoryBus, subject_matches};

#[test]
fn subject_wildcards() {
    assert!(subject_matches(
        "chelabase.rt.access.t1",
        "chelabase.rt.access.t1"
    ));
    assert!(subject_matches(
        "chelabase.rt.access.*",
        "chelabase.rt.access.t1"
    ));
    assert!(!subject_matches(
        "chelabase.rt.access.*",
        "chelabase.rt.access.t1.x"
    ));
    assert!(subject_matches(
        "chelabase.rt.presence.hb.*",
        "chelabase.rt.presence.hb.i-1"
    ));
    assert!(subject_matches(
        "chelabase.rt.>",
        "chelabase.rt.presence.hb.i-1"
    ));
    assert!(!subject_matches("chelabase.rt.>", "chelabase.rt"));
    assert!(!subject_matches(
        "chelabase.rt.access.t1",
        "chelabase.rt.access.t2"
    ));
    assert!(!subject_matches("chelabase.rt.*.t1", "chelabase.rt.access"));
    assert!(subject_matches(
        "chelabase.rt.*.t1",
        "chelabase.rt.access.t1"
    ));
}

#[tokio::test]
async fn messages_reach_every_matching_subscriber_with_their_reply() {
    let bus = MemoryBus::new();
    let mut exact = bus.subscribe("chelabase.rt.presence.sync.h").await.unwrap();
    let mut wild = bus.subscribe("chelabase.rt.presence.>").await.unwrap();
    let mut other = bus.subscribe("chelabase.rt.access.*").await.unwrap();
    let inbox = bus.new_inbox();
    assert!(inbox.starts_with("_INBOX_realtime."), "{inbox}");
    assert_ne!(inbox, bus.new_inbox());
    bus.publish(
        "chelabase.rt.presence.sync.h",
        Some(&inbox),
        Bytes::from_static(b"hi"),
    )
    .await
    .unwrap();
    for s in [&mut exact, &mut wild] {
        let m = tokio::time::timeout(Duration::from_secs(1), s.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.subject, "chelabase.rt.presence.sync.h");
        assert_eq!(m.reply.as_deref(), Some(inbox.as_str()));
        assert_eq!(m.payload, Bytes::from_static(b"hi"));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), other.next())
            .await
            .is_err(),
        "no match, no message"
    );
}

#[tokio::test]
async fn a_down_bus_fails_publishes_and_subscribes() {
    let bus = MemoryBus::new();
    bus.set_down(true);
    assert!(
        bus.publish("chelabase.rt.x", None, Bytes::new())
            .await
            .is_err()
    );
    assert!(bus.subscribe("chelabase.rt.x").await.is_err());
    bus.set_down(false);
    assert!(
        bus.publish("chelabase.rt.x", None, Bytes::new())
            .await
            .is_ok()
    );
}
