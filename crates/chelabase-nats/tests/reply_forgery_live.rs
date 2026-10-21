//! RT1 T1 audit (Critical): no NATS user can publish to a subject outside its allow list by
//! answering a message it sent itself with that subject as the reply (nats-server's
//! `allow_responses` checks no permission on a reply subject). Live: run in the stack's runner
//! (`just workbench test -p chelabase-nats --test reply_forgery_live -- --run-ignored only`),
//! with the same environment as `permissions_live`.

use async_nats::{Client, ConnectOptions, Event, ServerError};
use futures_util::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use uuid::Uuid;

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

async fn connect(user: &str) -> (Client, mpsc::UnboundedReceiver<String>) {
    let (tx, errors) = mpsc::unbounded_channel();
    let client = ConnectOptions::with_user_and_password(user.into(), password(user))
        .custom_inbox_prefix(format!("_INBOX_{user}"))
        .event_callback(move |event| {
            let tx = tx.clone();
            async move {
                if let Event::ServerError(err) = event {
                    let _ = tx.send(match err {
                        ServerError::Other(text) => text,
                        other => other.to_string(),
                    });
                }
            }
        })
        .connect(url())
        .await
        .unwrap_or_else(|e| panic!("connect as {user}: {e}"));
    assert!(
        client.server_info().auth_required,
        "the NATS server requires no authentication; no probe is sent"
    );
    (client, errors)
}

/// `user` publishes on `own` (a subject it may publish and subscribe to) with `forged` as the
/// reply subject, receives that message, and answers it. The answer must be refused (a
/// permissions violation naming `forged`) and never reach a runner subscriber.
async fn assert_reply_forgery_refused(user: &str, own: &str, forged: &str) {
    let (client, mut errors) = connect(user).await;
    let (runner, _) = connect("runner").await;
    let mut spy = runner.subscribe(forged.to_string()).await.unwrap();
    runner.flush().await.unwrap();
    let mut mine = client.subscribe(own.to_string()).await.unwrap();
    client.flush().await.unwrap();
    client
        .publish_with_reply(own.to_string(), forged.to_string(), "loop".into())
        .await
        .unwrap();
    client.flush().await.unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(1), mine.next())
        .await
        .expect("the message comes back")
        .expect("subscription open");
    assert_eq!(msg.reply.as_deref(), Some(forged));
    client
        .publish(forged.to_string(), "forged".into())
        .await
        .unwrap();
    client.flush().await.unwrap();
    let violation = tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(text) = errors.recv().await {
            if text.to_lowercase().contains("permissions violation")
                && text.contains(&format!("\"{forged}\""))
            {
                return true;
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    let delivered = tokio::time::timeout(Duration::from_millis(500), spy.next())
        .await
        .is_ok();
    assert!(violation, "{user}: answering on {forged} raised no permissions violation");
    assert!(!delivered, "{user}: forged a publish to {forged}");
}

#[tokio::test]
#[ignore]
async fn realtime_cannot_forge_through_a_reply_subject() {
    let own = format!("chelabase.rt.presence.sync.{}", Uuid::new_v4().simple());
    for forged in [
        format!("chelabase.rt.compute.stopping.{}", Uuid::new_v4()),
        format!("chelabase.rt.access.{}", Uuid::new_v4()),
        "chelabase.rpc.review".to_string(),
        "_INBOX_orchestrator.forged".to_string(),
    ] {
        assert_reply_forgery_refused("realtime", &own, &forged).await;
    }
}

#[tokio::test]
#[ignore]
async fn the_worker_cannot_forge_through_a_reply_subject() {
    // The worker publishes and hears `chelabase.member_roles.reconcile` (and resync).
    for forged in [
        format!("chelabase.rt.access.{}", Uuid::new_v4()),
        format!("chelabase.rt.presence.{}", Uuid::new_v4().simple()),
        "_INBOX_realtime.forged".to_string(),
    ] {
        assert_reply_forgery_refused("worker", "chelabase.member_roles.reconcile", &forged).await;
    }
}
