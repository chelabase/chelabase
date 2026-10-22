//! Carried gaps T4 (R7, P8, P11): the tenant-attached events on the stack's NATS. Live: run in
//! the stack's runner (`just workbench test -p chelabase-nats --test attach_events_live --
//! --run-ignored only`), connected as the runner (`CHELABASE_NATS_*` in its environment), which
//! runs the orchestrator and the worker in-process and so has both their rights. Every test uses
//! its own random tenants, so tests running at once (nextest runs each in its own process) never
//! see each other's events or consumers.

use std::time::{Duration, Instant};

use async_nats::jetstream::{self, consumer};
use chelabase_nats::{
    NatsConfig, STREAM_CONTROL, STREAM_NEON_EVENTS, TenantAttachedEvent, TenantAttachedShard,
    connect, ensure_neon_events_stream, neon_events_stream_config, publish_tenant_attached,
    tenant_attached_subject, wait_tenant_attached,
};
use futures_util::StreamExt;
use uuid::Uuid;

/// The runner's JetStream, with the events stream ensured (as the orchestrator does at start).
async fn js() -> jetstream::Context {
    let client = connect(&NatsConfig::from_env().expect("the runner's NATS config"))
        .await
        .expect("NATS reachable (run through just workbench test)");
    let js = jetstream::new(client);
    ensure_neon_events_stream(&js)
        .await
        .expect("ensure the events stream");
    js
}

/// A fresh tenant id: 32 lowercase hex.
fn tenant() -> String {
    Uuid::new_v4().simple().to_string()
}

fn event(tenant: &str) -> TenantAttachedEvent {
    TenantAttachedEvent {
        tenant_id: tenant.to_string(),
        shards: vec![TenantAttachedShard {
            node_id: 1,
            shard_number: 0,
        }],
        stripe_size: None,
        preferred_az: None,
    }
}

/// The names of the wait consumers of `tenant` on the events stream.
async fn wait_consumers(js: &jetstream::Context, tenant: &str) -> Vec<String> {
    let stream = js.get_stream(STREAM_NEON_EVENTS).await.unwrap();
    let names: Vec<String> = stream.consumer_names().map(|n| n.unwrap()).collect().await;
    let prefix = format!("attached-{tenant}-");
    names
        .into_iter()
        .filter(|n| n.starts_with(&prefix))
        .collect()
}

// The storage controller notifies ~2 ms before its 201 (red-team): the event is usually there
// before the worker starts waiting, and the wait finds it at once.
#[tokio::test]
#[ignore]
async fn an_event_published_before_the_wait_is_seen_at_once() {
    let js = js().await;
    let t = tenant();
    let sent = event(&t);
    publish_tenant_attached(&js, &sent).await.unwrap();
    let started = Instant::now();
    let got = wait_tenant_attached(&js, &t, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(got, Some(sent));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
#[ignore]
async fn an_event_published_during_the_wait_ends_it() {
    let js = js().await;
    let t = tenant();
    let sent = event(&t);
    let started = Instant::now();
    let wait = {
        let (js, t) = (js.clone(), t.clone());
        tokio::spawn(async move { wait_tenant_attached(&js, &t, Duration::from_secs(10)).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    publish_tenant_attached(&js, &sent).await.unwrap();
    let got = wait.await.unwrap().unwrap();
    assert_eq!(got, Some(sent));
    // Delivered as it lands, not at the next fetch's expiry or the deadline.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "took {:?}",
        started.elapsed()
    );
}

// Another tenant's event (one published before and one during the wait) doesn't end it; the
// wait gives up at about its timeout, not a fetch's expiry later.
#[tokio::test]
#[ignore]
async fn another_tenants_event_does_not_end_the_wait() {
    let js = js().await;
    let (mine, other) = (tenant(), tenant());
    publish_tenant_attached(&js, &event(&other)).await.unwrap();
    let started = Instant::now();
    let wait = {
        let (js, mine) = (js.clone(), mine.clone());
        tokio::spawn(async move { wait_tenant_attached(&js, &mine, Duration::from_secs(2)).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    publish_tenant_attached(&js, &event(&other)).await.unwrap();
    let got = wait.await.unwrap().unwrap();
    let took = started.elapsed();
    assert_eq!(got, None);
    assert!(
        took >= Duration::from_secs(2),
        "gave up early, after {took:?}"
    );
    assert!(took < Duration::from_secs(4), "the 2 s wait took {took:?}");
}

#[tokio::test]
#[ignore]
async fn two_waiters_on_one_tenant_both_see_its_event() {
    let js = js().await;
    let t = tenant();
    let sent = event(&t);
    let waiter = || {
        let (js, t) = (js.clone(), t.clone());
        tokio::spawn(async move { wait_tenant_attached(&js, &t, Duration::from_secs(10)).await })
    };
    let (a, b) = (waiter(), waiter());
    tokio::time::sleep(Duration::from_millis(300)).await;
    publish_tenant_attached(&js, &sent).await.unwrap();
    assert_eq!(a.await.unwrap().unwrap(), Some(sent.clone()));
    assert_eq!(b.await.unwrap().unwrap(), Some(sent));
    // And a third, after the fact, still reads it (limits retention: nothing was consumed).
    assert_eq!(
        wait_tenant_attached(&js, &t, Duration::from_secs(5))
            .await
            .unwrap(),
        Some(event(&t))
    );
}

// A5/A6: while a wait runs it has one consumer of its own, configured as P11 says; once the wait
// returns (event or timeout) the consumer is gone (deleted before the wait returns).
#[tokio::test]
#[ignore]
async fn a_waits_consumer_is_p11s_and_is_removed_when_it_returns() {
    let js = js().await;
    let t = tenant();
    assert!(wait_consumers(&js, &t).await.is_empty());

    let wait = {
        let (js, t) = (js.clone(), t.clone());
        tokio::spawn(async move { wait_tenant_attached(&js, &t, Duration::from_secs(4)).await })
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    let name = loop {
        if let Some(name) = wait_consumers(&js, &t).await.into_iter().next() {
            break name;
        }
        assert!(
            Instant::now() < deadline,
            "no consumer appeared for the wait"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        wait_consumers(&js, &t).await.len(),
        1,
        "one consumer per wait"
    );
    let stream = js.get_stream(STREAM_NEON_EVENTS).await.unwrap();
    let mut c: consumer::PullConsumer = stream.get_consumer(&name).await.unwrap();
    let config = c.info().await.unwrap().config.clone();
    assert_eq!(config.filter_subject, tenant_attached_subject(&t).unwrap());
    assert_eq!(
        config.deliver_policy,
        consumer::DeliverPolicy::LastPerSubject
    );
    assert_eq!(config.ack_policy, consumer::AckPolicy::None);
    assert!(config.memory_storage);
    assert_eq!(config.inactive_threshold, Duration::from_secs(30));
    assert_eq!(config.durable_name, None);

    assert_eq!(wait.await.unwrap().unwrap(), None, "no event: timed out");
    assert!(
        wait_consumers(&js, &t).await.is_empty(),
        "the timed-out wait left its consumer"
    );

    publish_tenant_attached(&js, &event(&t)).await.unwrap();
    assert!(
        wait_tenant_attached(&js, &t, Duration::from_secs(5))
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        wait_consumers(&js, &t).await.is_empty(),
        "the attached wait left its consumer"
    );
}

/// How many messages the stream holds for `subject` (a throwaway consumer's pending count).
async fn held_for(js: &jetstream::Context, subject: &str) -> u64 {
    let stream = js.get_stream(STREAM_NEON_EVENTS).await.unwrap();
    let name = format!("t4-count-{}", Uuid::new_v4().simple());
    let mut c: consumer::PullConsumer = stream
        .create_consumer(consumer::pull::Config {
            name: Some(name.clone()),
            filter_subject: subject.to_string(),
            deliver_policy: consumer::DeliverPolicy::All,
            ack_policy: consumer::AckPolicy::None,
            memory_storage: true,
            inactive_threshold: Duration::from_secs(30),
            ..Default::default()
        })
        .await
        .unwrap();
    let pending = c.info().await.unwrap().num_pending;
    stream.delete_consumer(&name).await.unwrap();
    pending
}

/// The stream sequence of the last message on `subject`, read through a throwaway consumer (the
/// runner may not `STREAM.MSG.GET`).
async fn last_seq(js: &jetstream::Context, subject: &str) -> u64 {
    let stream = js.get_stream(STREAM_NEON_EVENTS).await.unwrap();
    let name = format!("t4-last-{}", Uuid::new_v4().simple());
    let c: consumer::PullConsumer = stream
        .create_consumer(consumer::pull::Config {
            name: Some(name.clone()),
            filter_subject: subject.to_string(),
            deliver_policy: consumer::DeliverPolicy::LastPerSubject,
            ack_policy: consumer::AckPolicy::None,
            memory_storage: true,
            inactive_threshold: Duration::from_secs(30),
            ..Default::default()
        })
        .await
        .unwrap();
    let mut batch = c
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .unwrap();
    let message = batch
        .next()
        .await
        .expect("the subject's last message")
        .unwrap();
    let seq = message.info().unwrap().stream_sequence;
    stream.delete_consumer(&name).await.unwrap();
    seq
}

// P8: `ensure_neon_events_stream` creates the stream, is idempotent, puts back a config that
// drifted, and never touches the control stream. One message per subject is kept, and a second
// identical publish is stored (no `Nats-Msg-Id` dedupe: a re-attach after a restart must land).
// (Drift and the per-subject count share one test: the drift raises the per-subject limit for a
// moment.)
#[tokio::test]
#[ignore]
async fn ensure_is_idempotent_fixes_drift_and_keeps_one_message_per_tenant() {
    let js = js().await;
    let control_before = js
        .get_stream(STREAM_CONTROL)
        .await
        .unwrap()
        .info()
        .await
        .unwrap()
        .config
        .clone();
    ensure_neon_events_stream(&js).await.unwrap();
    ensure_neon_events_stream(&js).await.unwrap();

    let mut drifted = js
        .get_stream(STREAM_NEON_EVENTS)
        .await
        .unwrap()
        .info()
        .await
        .unwrap()
        .config
        .clone();
    drifted.max_age = Duration::from_secs(600);
    drifted.max_messages_per_subject = 5;
    drifted.max_messages = 10;
    js.update_stream(drifted).await.expect("drift the config");
    ensure_neon_events_stream(&js).await.unwrap();

    let want = neon_events_stream_config();
    let got = js
        .get_stream(STREAM_NEON_EVENTS)
        .await
        .unwrap()
        .info()
        .await
        .unwrap()
        .config
        .clone();
    assert_eq!(got.subjects, want.subjects);
    assert_eq!(got.retention, want.retention);
    assert_eq!(got.storage, want.storage);
    assert_eq!(got.max_messages_per_subject, want.max_messages_per_subject);
    assert_eq!(got.max_age, want.max_age);
    assert_eq!(got.max_messages, want.max_messages);
    assert_eq!(got.max_bytes, want.max_bytes);
    assert_eq!(got.discard, want.discard);
    assert_eq!(got.num_replicas, want.num_replicas);

    let control_after = js
        .get_stream(STREAM_CONTROL)
        .await
        .unwrap()
        .info()
        .await
        .unwrap()
        .config
        .clone();
    assert_eq!(
        control_after, control_before,
        "CHELABASE_CONTROL was changed"
    );

    let t = tenant();
    let subject = tenant_attached_subject(&t).unwrap();
    publish_tenant_attached(&js, &event(&t)).await.unwrap();
    let first = last_seq(&js, &subject).await;
    publish_tenant_attached(&js, &event(&t)).await.unwrap();
    let second = last_seq(&js, &subject).await;
    assert!(
        second > first,
        "the identical second event was deduplicated"
    );
    assert_eq!(held_for(&js, &subject).await, 1, "one message per tenant");
}
