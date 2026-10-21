//! RT1 T1 (R11, W13, S2, S3): every NATS user's permissions on the stack's server. Live: run in
//! the stack's runner (`just workbench test -p chelabase-nats --test permissions_live --
//! --run-ignored only`). Reads `CHELABASE_NATS_URL` and each user's password from
//! `$CHELABASE_KEYS_DIR/nats/<user>.password` (S1; the runner sees every NATS password file, it
//! is the test harness).
//!
//! S3: a refused core publish returns `Ok` on the client, so a refusal is proven by (1) the
//! server's permissions-violation error (`Event::ServerError`) within 1 s, and (2) no delivery
//! to a `runner` subscriber within 500 ms (for a JetStream subject: the control stream's last
//! sequence doesn't move). A refused subscribe is proven by the violation. Every test also checks
//! its positive control: what the user may do is delivered and raises no violation.

use async_nats::jetstream::{self, stream::StorageType};
use async_nats::{Client, ConnectOptions, Event, ServerError};
use chelabase_nats::{NatsConfig, STREAM_CONTROL, connect, rt};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

const VIOLATION_WAIT: Duration = Duration::from_secs(1);
const DELIVERY_WAIT: Duration = Duration::from_millis(500);

fn url() -> String {
    std::env::var("CHELABASE_NATS_URL").unwrap_or_else(|_| "nats://nats:4222".into())
}

fn password(user: &str) -> String {
    let keys = std::env::var("CHELABASE_KEYS_DIR").expect("CHELABASE_KEYS_DIR (stack.env)");
    let path = std::path::Path::new(&keys)
        .join("nats")
        .join(format!("{user}.password"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .trim_end()
        .to_string()
}

/// A client as `user`, with its own inbox prefix, whose server errors arrive on the receiver.
struct Probe {
    user: &'static str,
    client: Client,
    errors: mpsc::UnboundedReceiver<String>,
}

async fn probe(user: &'static str) -> Probe {
    let (tx, errors) = mpsc::unbounded_channel();
    let client = ConnectOptions::with_user_and_password(user.into(), password(user))
        .custom_inbox_prefix(format!("_INBOX_{user}"))
        .event_callback(move |event| {
            let tx = tx.clone();
            async move {
                if let Event::ServerError(err) = event {
                    let text = match err {
                        ServerError::Other(text) => text,
                        other => other.to_string(),
                    };
                    let _ = tx.send(text);
                }
            }
        })
        .connect(url())
        .await
        .unwrap_or_else(|e| panic!("connect as {user}: {e}"));
    // Without auth every probe below would go through: deletes of CHELABASE_CONTROL, jobs and
    // RPCs the worker would run. Refuse to send anything to a server that requires no auth.
    assert!(
        client.server_info().auth_required,
        "the NATS server requires no authentication (anonymous access is on); no probe is sent"
    );
    Probe {
        user,
        client,
        errors,
    }
}

impl Probe {
    /// The permissions violation naming `subject` (publish or subscription), if one arrives
    /// within 1 s. Other errors are kept out of the way but don't count.
    async fn violation(&mut self, kind: &str, subject: &str) -> Option<String> {
        let deadline = tokio::time::Instant::now() + VIOLATION_WAIT;
        loop {
            match tokio::time::timeout_at(deadline, self.errors.recv()).await {
                Ok(Some(text)) => {
                    let lower = text.to_lowercase();
                    if lower.contains("permissions violation")
                        && lower.contains(&kind.to_lowercase())
                        && text.contains(&format!("\"{subject}\""))
                    {
                        return Some(text);
                    }
                }
                Ok(None) | Err(_) => return None,
            }
        }
    }

    async fn publish(&self, subject: &str) {
        self.client
            .publish(subject.to_string(), "{}".into())
            .await
            .unwrap();
        self.client.flush().await.unwrap();
    }

    async fn assert_publish_refused(&mut self, subject: &str) {
        assert!(
            self.violation("publish", subject).await.is_some(),
            "{}: publishing {subject} raised no permissions violation",
            self.user
        );
    }

    async fn assert_no_violation(&mut self, kind: &str, subject: &str) {
        if let Some(text) = self.violation(kind, subject).await {
            panic!("{}: {kind} {subject} was refused: {text}", self.user);
        }
    }

    async fn assert_subscribe_refused(&mut self, subject: &str) {
        let _sub = self.client.subscribe(subject.to_string()).await.unwrap();
        self.client.flush().await.unwrap();
        assert!(
            self.violation("subscription", subject).await.is_some(),
            "{}: subscribing {subject} raised no permissions violation",
            self.user
        );
    }
}

/// Whether a `runner` subscriber on `filter` gets a message on exactly `subject` after `send`.
async fn runner_receives(
    runner: &mut Probe,
    filter: &str,
    subject: &str,
    send: impl std::future::Future<Output = ()>,
) -> bool {
    let mut sub = runner.client.subscribe(filter.to_string()).await.unwrap();
    runner.client.flush().await.unwrap();
    runner.assert_no_violation("subscription", filter).await;
    send.await;
    let got = tokio::time::timeout(DELIVERY_WAIT, async {
        while let Some(m) = sub.next().await {
            if m.subject.as_str() == subject {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    let _ = sub.unsubscribe().await;
    got
}

/// S3 for a core subject the runner may subscribe to: violation, and no delivery.
async fn assert_core_publish_refused(
    who: &mut Probe,
    runner: &mut Probe,
    filter: &str,
    subject: &str,
) {
    let delivered = runner_receives(runner, filter, subject, who.publish(subject)).await;
    who.assert_publish_refused(subject).await;
    assert!(
        !delivered,
        "{}: {subject} was refused but delivered",
        who.user
    );
}

/// S3 for a control-stream subject: violation, and the stream's last sequence doesn't move.
async fn assert_job_publish_refused(who: &mut Probe, subject: &str) {
    let runner = probe("runner").await;
    let js = jetstream::new(runner.client.clone());
    let before = js
        .get_stream(STREAM_CONTROL)
        .await
        .unwrap()
        .info()
        .await
        .unwrap()
        .state
        .last_sequence;
    who.publish(subject).await;
    who.assert_publish_refused(subject).await;
    tokio::time::sleep(DELIVERY_WAIT).await;
    let after = js
        .get_stream(STREAM_CONTROL)
        .await
        .unwrap()
        .info()
        .await
        .unwrap()
        .state
        .last_sequence;
    assert_eq!(
        before, after,
        "{}: {subject} was refused but reached {STREAM_CONTROL}",
        who.user
    );
}

/// S3 for a positive control: no violation, and the runner gets it.
async fn assert_core_publish_allowed(
    who: &mut Probe,
    runner: &mut Probe,
    filter: &str,
    subject: &str,
) {
    let delivered = runner_receives(runner, filter, subject, who.publish(subject)).await;
    who.assert_no_violation("publish", subject).await;
    assert!(delivered, "{}: {subject} was not delivered", who.user);
}

fn id() -> Uuid {
    Uuid::new_v4()
}

// --- authentication -------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn anonymous_connect_is_refused() {
    let err = ConnectOptions::new()
        .connect(url())
        .await
        .expect_err("anonymous access is off");
    assert!(
        matches!(
            err.kind(),
            async_nats::ConnectErrorKind::AuthorizationViolation
                | async_nats::ConnectErrorKind::Authentication
        ),
        "{err:?}"
    );
}

#[tokio::test]
#[ignore]
async fn a_wrong_password_or_unknown_user_is_refused() {
    for (user, pass) in [
        ("realtime", "not-the-password"),
        ("default", "x"),
        ("admin", "admin"),
    ] {
        let err = ConnectOptions::with_user_and_password(user.into(), pass.into())
            .connect(url())
            .await
            .expect_err(user);
        assert!(
            matches!(
                err.kind(),
                async_nats::ConnectErrorKind::AuthorizationViolation
                    | async_nats::ConnectErrorKind::Authentication
            ),
            "{user}: {err:?}"
        );
    }
}

// --- realtime (S2) --------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn realtime_cannot_publish_control_jobs() {
    let mut realtime = probe("realtime").await;
    for subject in [
        "chelabase.provision",
        "chelabase.delete",
        "chelabase.branch",
        "chelabase.merge",
    ] {
        assert_job_publish_refused(&mut realtime, subject).await;
    }
}

#[tokio::test]
#[ignore]
async fn realtime_cannot_call_or_trigger_the_worker() {
    let mut realtime = probe("realtime").await;
    let mut runner = probe("runner").await;
    for subject in [
        "chelabase.rpc.check_data_role",
        "chelabase.rpc.review",
        "chelabase.rpc.roles.create",
        "chelabase.rpc.member_roles.reconcile",
    ] {
        assert_core_publish_refused(&mut realtime, &mut runner, subject, subject).await;
    }
    for subject in [
        "chelabase.resync.branch",
        "chelabase.member_roles.reconcile",
    ] {
        assert_core_publish_refused(&mut realtime, &mut runner, subject, subject).await;
    }
}

// S2: realtime may not forge access, project or compute-stopping events (a forged
// compute-stopping would drop every slot).
#[tokio::test]
#[ignore]
async fn realtime_cannot_forge_platform_events() {
    let mut realtime = probe("realtime").await;
    let mut runner = probe("runner").await;
    for prefix in [rt::ACCESS, rt::PROJECT, rt::COMPUTE_STOPPING] {
        let subject = format!("{prefix}.{}", id());
        assert_core_publish_refused(&mut realtime, &mut runner, "chelabase.rt.>", &subject).await;
    }
}

#[tokio::test]
#[ignore]
async fn realtime_cannot_read_or_write_other_inboxes() {
    let mut realtime = probe("realtime").await;
    for subject in [
        "_INBOX.>",
        "_INBOX_orchestrator.>",
        "_INBOX_worker.>",
        "_INBOX_runner.>",
        ">",
        "chelabase.>",
    ] {
        realtime.assert_subscribe_refused(subject).await;
    }
    // Unsolicited writes to another service's inbox (allow_responses covers replies only).
    for subject in ["_INBOX_orchestrator.forged", "_INBOX.forged"] {
        realtime.publish(subject).await;
        realtime.assert_publish_refused(subject).await;
    }
}

#[tokio::test]
#[ignore]
async fn realtime_cannot_touch_the_control_stream_or_other_buckets() {
    let mut realtime = probe("realtime").await;
    for subject in [
        "$JS.API.STREAM.DELETE.CHELABASE_CONTROL",
        "$JS.API.STREAM.PURGE.CHELABASE_CONTROL",
        "$JS.API.STREAM.UPDATE.CHELABASE_CONTROL",
        "$JS.API.STREAM.INFO.CHELABASE_CONTROL",
        "$JS.API.CONSUMER.CREATE.CHELABASE_CONTROL.x",
        "$JS.API.CONSUMER.DELETE.CHELABASE_CONTROL.chelabase-worker-provision",
        "$JS.API.STREAM.CREATE.KV_SOMETHING_ELSE",
        "$JS.API.STREAM.DELETE.KV_CHELABASE_RT_LEASES",
        "$JS.API.STREAM.PURGE.KV_CHELABASE_RT_SLOTS",
        "$JS.API.STREAM.LIST",
        "$JS.API.STREAM.NAMES",
        "$JS.ACK.CHELABASE_CONTROL.x.1.1.1.1.0",
        "$KV.OTHER_BUCKET.key",
    ] {
        realtime.publish(subject).await;
        realtime.assert_publish_refused(subject).await;
    }
    // The stream is still there.
    let runner = probe("runner").await;
    jetstream::new(runner.client.clone())
        .get_stream(STREAM_CONTROL)
        .await
        .expect("CHELABASE_CONTROL exists");
}

#[tokio::test]
#[ignore]
async fn realtime_publishes_rows_watch_and_presence() {
    let mut realtime = probe("realtime").await;
    let mut runner = probe("runner").await;
    for subject in [
        format!("{}.{}.16384.16390", rt::ROWS, id()),
        format!("{}.{}.16384", rt::WATCH, id()),
        format!("{}.{}", rt::PRESENCE, "0123abcd"),
        format!("{}.hb.{}", rt::PRESENCE, id()),
    ] {
        assert_core_publish_allowed(&mut realtime, &mut runner, "chelabase.rt.>", &subject).await;
    }
}

// Realtime hears every `chelabase.rt.>` subject (platform events included) and answers a
// presence sync request from another instance (allow_responses).
#[tokio::test]
#[ignore]
async fn realtime_subscribes_rt_and_answers_requests() {
    let mut realtime = probe("realtime").await;
    let mut orchestrator = probe("orchestrator").await;
    let mut sub = realtime.client.subscribe("chelabase.rt.>").await.unwrap();
    realtime.client.flush().await.unwrap();
    realtime
        .assert_no_violation("subscription", "chelabase.rt.>")
        .await;
    let subject = format!("{}.{}", rt::PROJECT, id());
    orchestrator.publish(&subject).await;
    orchestrator.assert_no_violation("publish", &subject).await;
    // Other tests publish under `chelabase.rt.>` at the same time: wait for this one's subject.
    tokio::time::timeout(DELIVERY_WAIT, async {
        while let Some(m) = sub.next().await {
            if m.subject.as_str() == subject {
                return;
            }
        }
    })
    .await
    .expect("realtime hears the orchestrator's project event");
    // nats-server 2.15 refuses the reply when the request reached the responder's connection
    // through two overlapping subscriptions (allow_responses' one response is spent): answer from
    // a connection whose only matching subscription is the sync subject.
    sub.unsubscribe().await.unwrap();
    realtime.client.flush().await.unwrap();

    let sync = format!("{}.sync.{}", rt::PRESENCE, "feedbeef");
    let mut requests = realtime.client.subscribe(sync.clone()).await.unwrap();
    realtime.client.flush().await.unwrap();
    let responder = realtime.client.clone();
    tokio::spawn(async move {
        if let Some(m) = requests.next().await {
            responder
                .publish(m.reply.unwrap(), "snapshot".into())
                .await
                .unwrap();
            responder.flush().await.unwrap();
        }
    });
    let runner = probe("runner").await;
    let reply = tokio::time::timeout(
        Duration::from_secs(3),
        runner.client.request(sync, "{}".into()),
    )
    .await
    .expect("an answer in time")
    .expect("realtime answered");
    assert_eq!(&reply.payload[..], b"snapshot");
}

// S2/S4: realtime makes, through the crate's own connect (credentials and `_INBOX_realtime`),
// every JetStream call a KV bucket makes on its two buckets: `$JS.API.INFO`, stream
// INFO/CREATE/UPDATE with S4's options (max_age 10 s, subject delete markers), `$KV.<bucket>.>`
// writes with acks, direct gets, and the ordered push consumer a watch or key listing creates and
// deletes. The `async-nats` `kv` feature is off in the workspace (T7/T8 turn it on; S7: T1
// changes no Cargo.toml), so this drives the stream-level API the `kv` module wraps.
#[tokio::test]
#[ignore]
async fn realtime_drives_its_two_kv_buckets() {
    use async_nats::jetstream::consumer::push;
    use async_nats::jetstream::stream::{self, DiscardPolicy};
    let client = connect(&NatsConfig {
        url: url(),
        user: Some("realtime".into()),
        password: Some(password("realtime")),
        inbox_prefix: Some("_INBOX_realtime".into()),
    })
    .await
    .expect("realtime connects with its credentials");
    let js = jetstream::new(client.clone());
    js.query_account().await.expect("$JS.API.INFO");
    for bucket in [rt::LEASES_BUCKET, rt::SLOTS_BUCKET] {
        let name = format!("KV_{bucket}");
        let config = || stream::Config {
            name: name.clone(),
            subjects: vec![format!("$KV.{bucket}.>")],
            max_messages_per_subject: 1,
            max_age: Duration::from_secs(10),
            storage: StorageType::Memory,
            num_replicas: 1,
            allow_rollup: true,
            deny_delete: true,
            allow_direct: true,
            discard: DiscardPolicy::New,
            allow_message_ttl: true,
            subject_delete_marker_ttl: Some(Duration::from_secs(5)),
            ..Default::default()
        };
        js.create_or_update_stream(config())
            .await
            .unwrap_or_else(|e| panic!("{name}: create: {e}"));
        // A second run updates (STREAM.UPDATE) rather than creates.
        js.update_stream(config())
            .await
            .unwrap_or_else(|e| panic!("{name}: update: {e}"));
        let stream = js
            .get_stream(&name)
            .await
            .unwrap_or_else(|e| panic!("{name}: info: {e}"));

        let subject = format!("$KV.{bucket}.{}.0", id());
        let consumer: async_nats::jetstream::consumer::Consumer<push::OrderedConfig> = stream
            .create_consumer(push::OrderedConfig {
                deliver_subject: client.new_inbox(),
                filter_subject: subject.clone(),
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("{name}: ordered consumer: {e}"));
        let mut messages = consumer.messages().await.unwrap();
        js.publish(subject.clone(), "holder-a".into())
            .await
            .unwrap_or_else(|e| panic!("{subject}: publish: {e}"))
            .await
            .unwrap_or_else(|e| panic!("{subject}: ack: {e}"));
        let got = tokio::time::timeout(Duration::from_secs(3), messages.next())
            .await
            .expect("the ordered consumer delivers")
            .unwrap()
            .unwrap();
        assert_eq!(&got.payload[..], b"holder-a");
        let last = stream
            .direct_get_last_for_subject(subject.clone())
            .await
            .unwrap_or_else(|e| panic!("{name}: direct get: {e}"));
        assert_eq!(&last.payload[..], b"holder-a");
        let consumer_name = format!("t1-probe-{}", id());
        let _durable: async_nats::jetstream::consumer::PullConsumer = stream
            .create_consumer(async_nats::jetstream::consumer::pull::Config {
                name: Some(consumer_name.clone()),
                filter_subject: subject.clone(),
                ..Default::default()
            })
            .await
            .unwrap_or_else(|e| panic!("{name}: consumer create: {e}"));
        stream
            .delete_consumer(&consumer_name)
            .await
            .unwrap_or_else(|e| panic!("{name}: consumer delete: {e}"));
    }
    // Flow-control replies of those watches (`$JS.FC.<stream>.>`) are allowed.
    let mut realtime = probe("realtime").await;
    for subject in [
        "$JS.FC.KV_CHELABASE_RT_LEASES.x.1",
        "$JS.FC.KV_CHELABASE_RT_SLOTS.x.1",
    ] {
        realtime.publish(subject).await;
        realtime.assert_no_violation("publish", subject).await;
    }
}

// --- orchestrator and worker (S2) -----------------------------------------------------------

#[tokio::test]
#[ignore]
async fn the_orchestrator_publishes_project_and_access_events_only() {
    let mut orchestrator = probe("orchestrator").await;
    let mut runner = probe("runner").await;
    for prefix in [rt::PROJECT, rt::ACCESS] {
        let subject = format!("{prefix}.{}", id());
        assert_core_publish_allowed(&mut orchestrator, &mut runner, "chelabase.rt.>", &subject)
            .await;
    }
    for subject in [
        format!("{}.{}", rt::COMPUTE_STOPPING, id()),
        format!("{}.{}.1.2", rt::ROWS, id()),
        format!("{}.{}.1", rt::WATCH, id()),
        format!("{}.x", rt::PRESENCE),
    ] {
        assert_core_publish_refused(&mut orchestrator, &mut runner, "chelabase.rt.>", &subject)
            .await;
    }
}

#[tokio::test]
#[ignore]
async fn the_worker_publishes_project_and_compute_stopping_events_only() {
    let mut worker = probe("worker").await;
    let mut runner = probe("runner").await;
    for prefix in [rt::PROJECT, rt::COMPUTE_STOPPING] {
        let subject = format!("{prefix}.{}", id());
        assert_core_publish_allowed(&mut worker, &mut runner, "chelabase.rt.>", &subject).await;
    }
    for subject in [
        format!("{}.{}", rt::ACCESS, id()),
        format!("{}.{}.1.2", rt::ROWS, id()),
        format!("{}.{}.1", rt::WATCH, id()),
        format!("{}.x", rt::PRESENCE),
    ] {
        assert_core_publish_refused(&mut worker, &mut runner, "chelabase.rt.>", &subject).await;
    }
}

#[tokio::test]
#[ignore]
async fn no_user_reads_the_shared_inbox_or_another_services_inbox() {
    for (user, others) in [
        (
            "orchestrator",
            &["_INBOX.>", "_INBOX_worker.>", "_INBOX_realtime.>", ">"][..],
        ),
        (
            "worker",
            &[
                "_INBOX.>",
                "_INBOX_orchestrator.>",
                "_INBOX_realtime.>",
                ">",
            ][..],
        ),
        ("runner", &["_INBOX.>", ">"][..]),
    ] {
        let mut p = probe(user).await;
        for subject in others {
            p.assert_subscribe_refused(subject).await;
        }
    }
}

#[tokio::test]
#[ignore]
async fn no_user_deletes_or_purges_the_control_stream() {
    for user in ["orchestrator", "worker", "runner"] {
        let mut p = probe(user).await;
        for subject in [
            "$JS.API.STREAM.DELETE.CHELABASE_CONTROL",
            "$JS.API.STREAM.PURGE.CHELABASE_CONTROL",
        ] {
            p.publish(subject).await;
            p.assert_publish_refused(subject).await;
        }
    }
}

// Only realtime (and the runner, which runs it in-process) touches realtime's buckets.
#[tokio::test]
#[ignore]
async fn the_orchestrator_and_worker_cannot_touch_realtimes_buckets() {
    for user in ["orchestrator", "worker"] {
        let mut p = probe(user).await;
        for subject in [
            "$KV.CHELABASE_RT_LEASES.x",
            "$KV.CHELABASE_RT_SLOTS.x",
            "$JS.API.STREAM.INFO.KV_CHELABASE_RT_LEASES",
            "$JS.API.STREAM.DELETE.KV_CHELABASE_RT_SLOTS",
            "$JS.API.DIRECT.GET.KV_CHELABASE_RT_LEASES.$KV.CHELABASE_RT_LEASES.x",
        ] {
            p.publish(subject).await;
            p.assert_publish_refused(subject).await;
        }
    }
}

// The orchestrator still reads its control stream (ensure_control_stream), with its own inbox.
#[tokio::test]
#[ignore]
async fn the_orchestrator_still_reads_the_control_stream() {
    let client = connect(&NatsConfig {
        url: url(),
        user: Some("orchestrator".into()),
        password: Some(password("orchestrator")),
        inbox_prefix: Some("_INBOX_orchestrator".into()),
    })
    .await
    .expect("the orchestrator connects with its credentials");
    jetstream::new(client)
        .get_stream(STREAM_CONTROL)
        .await
        .expect("STREAM.INFO on CHELABASE_CONTROL");
}
