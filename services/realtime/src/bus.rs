//! Core NATS between instances and from the platform's publishers (spec §1, §5): access and
//! project events in, presence out and in. The realtime NATS user may subscribe only to
//! `chelabase.rt.>` and `_INBOX_realtime.>` and publish presence, rows, watches and replies on
//! `_INBOX_realtime.>`/`_INBOX_runner.>` (S2 as amended: no `allow_responses`); a presence
//! snapshot request carries a reply subject from [`Bus::new_inbox`] and every instance answers
//! by publishing there. Note T1 ruling (g): nats-server refuses a reply when the request reached
//! the responder through two overlapping subscriptions on one connection, so realtime never
//! holds two subscriptions that match one subject.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BusMessage {
    pub subject: String,
    pub reply: Option<String>,
    pub payload: Bytes,
}

pub type BusStream = Pin<Box<dyn Stream<Item = BusMessage> + Send>>;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("nats: {0}")]
pub struct BusError(pub String);

#[async_trait]
pub trait Bus: Send + Sync + 'static {
    async fn publish(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<(), BusError>;
    /// Every message on subjects matching `subject` (NATS wildcards `*` and `>`) from now on.
    async fn subscribe(&self, subject: &str) -> Result<BusStream, BusError>;
    /// A fresh reply subject under this connection's inbox prefix.
    fn new_inbox(&self) -> String;
}

/// NATS subject matching: `*` matches one token, a final `>` one or more.
pub fn subject_matches(pattern: &str, subject: &str) -> bool {
    let mut subject_tokens = subject.split('.');
    let mut pattern_tokens = pattern.split('.').peekable();
    while let Some(p) = pattern_tokens.next() {
        if p == ">" && pattern_tokens.peek().is_none() {
            return subject_tokens.next().is_some();
        }
        match subject_tokens.next() {
            None => return false,
            Some(s) if p == "*" || p == s => {}
            Some(_) => return false,
        }
    }
    subject_tokens.next().is_none()
}

/// The production bus.
pub struct NatsBus {
    client: async_nats::Client,
}

impl NatsBus {
    pub fn new(client: async_nats::Client) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Bus for NatsBus {
    async fn publish(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<(), BusError> {
        let result = match reply {
            Some(reply) => {
                self.client
                    .publish_with_reply(subject.to_string(), reply.to_string(), payload)
                    .await
            }
            None => self.client.publish(subject.to_string(), payload).await,
        };
        result.map_err(|e| BusError(e.to_string()))
    }
    async fn subscribe(&self, subject: &str) -> Result<BusStream, BusError> {
        let sub = self
            .client
            .subscribe(subject.to_string())
            .await
            .map_err(|e| BusError(e.to_string()))?;
        Ok(Box::pin(sub.map(|m| BusMessage {
            subject: m.subject.to_string(),
            reply: m.reply.map(|r| r.to_string()),
            payload: m.payload,
        })))
    }
    fn new_inbox(&self) -> String {
        self.client.new_inbox()
    }
}

/// An in-process bus for tests, shared by every instance a test builds; inboxes are
/// `_INBOX_realtime.<random>`. While [`MemoryBus::set_down`] is on, publishes and subscribes
/// fail.
#[derive(Default)]
pub struct MemoryBus {
    subs: Mutex<Vec<(String, mpsc::UnboundedSender<BusMessage>)>>,
    down: AtomicBool,
}

impl MemoryBus {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

#[async_trait]
impl Bus for MemoryBus {
    async fn publish(
        &self,
        subject: &str,
        reply: Option<&str>,
        payload: Bytes,
    ) -> Result<(), BusError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(BusError("disconnected".into()));
        }
        let msg = BusMessage {
            subject: subject.to_string(),
            reply: reply.map(str::to_string),
            payload,
        };
        let mut subs = self.subs.lock().unwrap();
        // A dropped subscriber's sender is closed: it goes on the next publish.
        subs.retain(|(_, tx)| !tx.is_closed());
        for (pattern, tx) in subs.iter() {
            if subject_matches(pattern, subject) {
                let _ = tx.send(msg.clone());
            }
        }
        Ok(())
    }
    async fn subscribe(&self, subject: &str) -> Result<BusStream, BusError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(BusError("disconnected".into()));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        self.subs.lock().unwrap().push((subject.to_string(), tx));
        Ok(Box::pin(futures_util::stream::unfold(
            rx,
            |mut rx| async move { rx.recv().await.map(|m| (m, rx)) },
        )))
    }
    fn new_inbox(&self) -> String {
        format!("_INBOX_realtime.{}", uuid::Uuid::new_v4().simple())
    }
}
