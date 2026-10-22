//! Carried gaps T4 (R7, P7, P8, P11): the tenant-attached event's names, wire shape, stream and
//! consumer configs, and a wait that stays bounded when NATS is unreachable. The live behaviour
//! (history, timeouts, concurrent waiters, cleanup, drift) is in `tests/attach_events_live.rs`.

use std::time::Duration;

use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy};
use async_nats::jetstream::stream::{DiscardPolicy, RetentionPolicy, StorageType};
use serde_json::json;

use super::*;

const T: &str = "0123456789abcdef0123456789abcdef";

#[test]
fn the_attached_subject_is_one_token_per_tenant() {
    assert_eq!(STREAM_NEON_EVENTS, "CHELABASE_NEON_EVENTS");
    assert_eq!(
        tenant_attached_subject(T).unwrap(),
        format!("chelabase.neon.attached.{T}")
    );
    let other = "ffffffffffffffffffffffffffffffff";
    assert_ne!(
        tenant_attached_subject(T).unwrap(),
        tenant_attached_subject(other).unwrap()
    );
}

// P7: 32 lowercase hex only. Anything else could widen the subject (`*`, `>`), add a token
// (`.`), or name another tenant's subject in a different spelling (upper case).
#[test]
fn the_attached_subject_takes_32_lowercase_hex_only() {
    let upper = T.to_uppercase();
    let one_upper = "0123456789abcdeF0123456789abcdef";
    let longer = format!("{T}0");
    let bad: Vec<&str> = vec![
        "",
        &upper,
        one_upper,
        &T[..31],
        &longer,
        "0123456789abcdef0123456789abcde.",
        "0123456789abcdef.123456789abcdef",
        "0123456789abcdef0123456789abcde*",
        "0123456789abcdef0123456789abcde>",
        "*",
        ">",
        "0123456789abcdef0123456789abcdeg",
        " 123456789abcdef0123456789abcdef",
        "0123456789abcdef0123456789abcde\n",
    ];
    for tenant in bad {
        assert!(
            tenant_attached_subject(tenant).is_err(),
            "{tenant:?} was accepted"
        );
    }
}

// P7: the events are never control-stream jobs (a work queue would hand them to one consumer).
#[test]
fn the_attached_subject_is_not_a_control_subject() {
    for s in control_subjects() {
        assert!(!s.starts_with("chelabase.neon"), "{s}");
    }
    assert!(missing_subjects(&control_subjects()).is_empty());
}

// The event carries what the storage controller's notify-attach body carries, by the same names.
#[test]
fn the_event_has_the_hooks_wire_shape() {
    let event = TenantAttachedEvent {
        tenant_id: T.into(),
        shards: vec![
            TenantAttachedShard {
                node_id: 1,
                shard_number: 0,
            },
            TenantAttachedShard {
                node_id: 7,
                shard_number: 1,
            },
        ],
        stripe_size: Some(32768),
        preferred_az: Some("az-1".into()),
    };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(
        wire,
        json!({
            "tenant_id": T,
            "shards": [{ "node_id": 1, "shard_number": 0 }, { "node_id": 7, "shard_number": 1 }],
            "stripe_size": 32768,
            "preferred_az": "az-1",
        })
    );
    let back: TenantAttachedEvent = serde_json::from_value(wire).unwrap();
    assert_eq!(back, event);
    // The storage controller's own body (nulls for an unsharded tenant) parses as is.
    let storcon: TenantAttachedEvent = serde_json::from_value(json!({
        "tenant_id": T, "preferred_az": null, "stripe_size": null,
        "shards": [{ "node_id": 2, "shard_number": 0 }],
    }))
    .unwrap();
    assert_eq!(
        storcon,
        TenantAttachedEvent {
            tenant_id: T.into(),
            shards: vec![TenantAttachedShard {
                node_id: 2,
                shard_number: 0
            }],
            stripe_size: None,
            preferred_az: None,
        }
    );
    // Every field but the tenant defaults.
    let bare: TenantAttachedEvent = serde_json::from_value(json!({ "tenant_id": T })).unwrap();
    assert_eq!(
        bare,
        TenantAttachedEvent {
            tenant_id: T.into(),
            shards: vec![],
            stripe_size: None,
            preferred_az: None,
        }
    );
}

// P8: limits retention (every waiter reads the history; nothing is consumed away), one message
// per subject, an hour of history, bounded in count and bytes, the oldest dropped first.
#[test]
fn the_events_stream_config_is_p8s() {
    let c = neon_events_stream_config();
    assert_eq!(c.name, STREAM_NEON_EVENTS);
    assert_eq!(c.subjects, vec!["chelabase.neon.attached.*".to_string()]);
    assert_eq!(c.retention, RetentionPolicy::Limits);
    assert_eq!(c.storage, StorageType::File);
    assert_eq!(c.max_messages_per_subject, 1);
    assert_eq!(c.max_age, Duration::from_secs(3600));
    assert_eq!(c.max_messages, 100_000);
    assert_eq!(c.max_bytes, 64 * 1024 * 1024);
    assert_eq!(c.discard, DiscardPolicy::Old);
    assert_eq!(c.num_replicas, 1);
    // The stream's subject covers every tenant's subject, and only those.
    let subject = tenant_attached_subject(T).unwrap();
    assert_eq!(subject.split('.').count(), c.subjects[0].split('.').count());
    assert!(subject.starts_with(c.subjects[0].trim_end_matches('*')));
}

// P11 (A5, A6): a named consumer per wait, filtered to the one tenant's subject, reading the last
// message for it (published before the wait or not), no acks, in memory, removed by the server
// 30 s after its waiter is gone.
#[test]
fn the_wait_consumer_config_is_p11s() {
    let c = tenant_attached_consumer_config(T).unwrap();
    let name = c.name.clone().expect("a named consumer");
    let suffix = name
        .strip_prefix(&format!("attached-{T}-"))
        .unwrap_or_else(|| panic!("{name}"));
    assert_eq!(suffix.len(), 8, "{name}");
    assert!(
        suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{name}"
    );
    assert_eq!(c.durable_name, None);
    assert_eq!(c.filter_subject, tenant_attached_subject(T).unwrap());
    assert!(c.filter_subjects.is_empty());
    assert_eq!(c.deliver_policy, DeliverPolicy::LastPerSubject);
    assert_eq!(c.ack_policy, AckPolicy::None);
    assert!(c.memory_storage);
    assert_eq!(c.inactive_threshold, Duration::from_secs(30));
    // Two waiters on one tenant get two consumers.
    let again = tenant_attached_consumer_config(T).unwrap();
    assert_ne!(again.name, c.name);
    assert!(tenant_attached_consumer_config("NOT-A-TENANT").is_err());
}

/// A client whose server never answers (nothing listens on port 1).
async fn unreachable_js() -> async_nats::jetstream::Context {
    let client = async_nats::ConnectOptions::new()
        .retry_on_initial_connect()
        .connect("nats://127.0.0.1:1")
        .await
        .expect("a client that connects in the background");
    async_nats::jetstream::new(client)
}

// NATS down for the whole wait: the wait still ends at its deadline (each NATS call bounded by
// what is left of it), with the error, so the worker can say why it didn't wait.
#[tokio::test]
async fn a_wait_with_nats_down_ends_at_its_deadline_with_an_error() {
    let js = unreachable_js().await;
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        wait_tenant_attached(&js, T, Duration::from_secs(1)),
    )
    .await
    .expect("the wait ended within 10 s");
    let took = started.elapsed();
    assert!(result.is_err(), "{result:?}");
    assert!(took < Duration::from_secs(3), "the 1 s wait took {took:?}");
}

#[tokio::test]
async fn a_wait_for_an_invalid_tenant_is_refused_at_once() {
    let js = unreachable_js().await;
    let started = std::time::Instant::now();
    let result = wait_tenant_attached(&js, "chelabase.>", Duration::from_secs(5)).await;
    assert!(result.is_err(), "{result:?}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

fn js_refusal(code: usize, err_code: u64) -> async_nats::jetstream::stream::ConsumerErrorKind {
    let e: async_nats::jetstream::Error = serde_json::from_value(
        json!({ "code": code, "err_code": err_code, "description": "refused" }),
    )
    .unwrap();
    async_nats::jetstream::stream::ConsumerErrorKind::JetStream(e)
}

// P3: a refusal that can't change within the wait ends it; anything else is retried.
#[test]
fn permanent_consumer_refusals_are_told_from_transient_ones() {
    use async_nats::jetstream::stream::ConsumerErrorKind as K;
    // STREAM_NOT_FOUND (10059), JETSTREAM_NOT_ENABLED (10076), ..._FOR_ACCOUNT (10039).
    assert!(consumer_create_refusal_is_permanent(&js_refusal(
        404, 10059
    )));
    assert!(consumer_create_refusal_is_permanent(&js_refusal(
        503, 10076
    )));
    assert!(consumer_create_refusal_is_permanent(&js_refusal(
        503, 10039
    )));
    // Other API errors (e.g. resource limits, a leader election) may pass.
    assert!(!consumer_create_refusal_is_permanent(&js_refusal(
        500, 10008
    )));
    assert!(!consumer_create_refusal_is_permanent(&js_refusal(503, 0)));
    // A permissions violation is a silently dropped request: a timeout, like a slow server.
    assert!(!consumer_create_refusal_is_permanent(&K::TimedOut));
    assert!(!consumer_create_refusal_is_permanent(&K::Request));
    assert!(!consumer_create_refusal_is_permanent(&K::Other));
}

// Item 4: pulls that keep failing are a failure, not a timeout.
#[test]
fn a_wait_whose_pulls_keep_failing_is_a_failure() {
    // No pull ever answered (create alone doesn't count): failed, whatever the count.
    assert!(attach_wait_failed(false, 0));
    assert!(attach_wait_failed(false, 5));
    // A pull answered, and the last calls were not all failures: a plain timeout.
    assert!(!attach_wait_failed(true, 0));
    assert!(!attach_wait_failed(true, ATTACH_FAILURE_LIMIT - 1));
    // The last ATTACH_FAILURE_LIMIT calls all failed after an answered pull: failed.
    assert!(attach_wait_failed(true, ATTACH_FAILURE_LIMIT));
    assert!(attach_wait_failed(true, ATTACH_FAILURE_LIMIT + 4));
    assert_eq!(ATTACH_FAILURE_LIMIT, 3);
}

// Rule 4's other side: a pull that ran until the deadline is no NATS answer, so a wait with no
// event and one long pull (a 1 s wait: the server's batch expiry used to land at the deadline,
// which cut the pull off first) was an `Err`. Every pull now expires before the deadline, by
// `ATTACH_PULL_MARGIN` (half what is left, for a short wait), so the server's empty-batch answer
// arrives within it; a pull too short to ask for isn't asked.
#[test]
fn every_pull_expires_before_the_deadline_so_the_server_answers_it() {
    let ms = Duration::from_millis;
    // A long wait: the longest pull, well before the deadline.
    assert_eq!(attach_pull_expires(Duration::from_secs(60)), Some(ATTACH_FETCH_EXPIRES));
    assert_eq!(
        attach_pull_expires(ATTACH_FETCH_EXPIRES + ATTACH_PULL_MARGIN),
        Some(ATTACH_FETCH_EXPIRES)
    );
    // The last pull of a wait, and a 1 s wait's only pull: it ends ATTACH_PULL_MARGIN early.
    assert_eq!(attach_pull_expires(ms(1000)), Some(ms(1000) - ATTACH_PULL_MARGIN));
    assert_eq!(attach_pull_expires(ms(2000)), Some(ms(2000) - ATTACH_PULL_MARGIN));
    // A short wait keeps half of what is left for the answer.
    assert_eq!(attach_pull_expires(ms(100)), Some(ms(50)));
    assert_eq!(attach_pull_expires(ATTACH_FETCH_MIN * 2), Some(ATTACH_FETCH_MIN));
    // Too little left for a pull worth asking for: none.
    assert_eq!(attach_pull_expires(ATTACH_FETCH_MIN * 2 - ms(1)), None);
    assert_eq!(attach_pull_expires(ms(0)), None);
    // Exhaustively over 0..10 s in 1 ms steps: a pull ends before the deadline with room for the
    // answer, and is never shorter than ATTACH_FETCH_MIN nor longer than ATTACH_FETCH_EXPIRES.
    for left in (0..10_000).map(ms) {
        if let Some(expires) = attach_pull_expires(left) {
            assert!(expires >= ATTACH_FETCH_MIN, "{left:?}: {expires:?}");
            assert!(expires <= ATTACH_FETCH_EXPIRES, "{left:?}: {expires:?}");
            assert!(
                left - expires >= ATTACH_PULL_MARGIN.min(left / 2),
                "{left:?}: {expires:?} leaves no room for the answer"
            );
        } else {
            assert!(left < ATTACH_FETCH_MIN * 2, "{left:?}: no pull");
        }
    }
    assert_eq!(ATTACH_PULL_MARGIN, ms(250));
}

// Item 3: at most 2 tries.
#[test]
fn a_refused_create_is_tried_at_most_twice() {
    assert_eq!(ATTACH_REFUSAL_LIMIT, 2);
}

// Item 2: the pause between tries never outlasts the deadline.
#[tokio::test]
async fn the_retry_pause_is_bounded_by_the_deadline() {
    let started = tokio::time::Instant::now();
    pause_until(started + Duration::from_millis(30)).await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(25) && took < ATTACH_RETRY_PAUSE,
        "{took:?}"
    );
    let started = tokio::time::Instant::now();
    pause_until(started + Duration::from_secs(5)).await;
    let took = started.elapsed();
    assert!(
        took >= Duration::from_millis(240) && took < Duration::from_secs(1),
        "{took:?}"
    );
}
