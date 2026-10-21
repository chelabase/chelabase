//! One instance's presence for one channel (spec §5.2, R4): every member across instances (a
//! [`ChannelPresence`]), the local viewers it fans out to, and the bus traffic for the channel's
//! hash: members in and out on `chelabase.rt.presence.<hash>`, snapshot requests answered on
//! `chelabase.rt.presence.sync.<hash>`, and the instance's own sync request when it starts.
//!
//! An instance whose members this hub dropped (its heartbeats stopped: a crash, or a
//! partition) and that is heard from again is asked for its members anew: after a partition
//! heals, every side gets the other's members back without anyone changing their state.
//!
//! A viewer that registers while the start-up sync is still collecting answers gets, once the
//! sync is over, the members as they were when it registered plus every snapshot answer, then
//! every change that happened since, in order. Visibility (R15) is the viewer's own business
//! (`socket`): the hub hands out raw members.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::bus::{Bus, BusStream};
use crate::channel::Scope;
use crate::presence::{
    ChannelPresence, Diff, PresenceError, PresenceEvent, PresenceMsg, presence_subject,
    sync_subject,
};
use crate::protocol::Member;

/// What a viewer gets from its hub.
#[derive(Debug, Clone)]
pub(crate) enum HubEvent {
    /// Every member (first, once).
    State(Vec<Member>),
    Diff(Diff),
}

/// A sync request's body.
#[derive(Serialize, Deserialize)]
struct SyncRequest {
    instance: String,
}

struct Pending {
    base: BTreeMap<String, Member>,
    queue: Vec<Diff>,
}

struct Viewer {
    tx: mpsc::UnboundedSender<HubEvent>,
    /// Until the start-up sync is over.
    pending: Option<Pending>,
}

struct State {
    presence: ChannelPresence,
    viewers: HashMap<u64, Viewer>,
    synced: bool,
    /// Instances whose members expired: heard from again, they're asked for them.
    lost: HashSet<String>,
}

pub(crate) struct HubInner {
    scope: Scope,
    channel: String,
    key: String,
    instance: String,
    bus: Arc<dyn Bus>,
    state: Mutex<State>,
    /// Asks the hub's task to send a sync request (a lost instance is back).
    resync: mpsc::UnboundedSender<()>,
}

/// The hub of one channel hash; dropping it stops its bus traffic.
pub(crate) struct Hub {
    inner: Arc<HubInner>,
    task: AbortHandle,
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn apply_to_base(base: &mut BTreeMap<String, Member>, diff: &Diff) {
    for m in &diff.leaves {
        base.remove(&m.id);
    }
    for m in &diff.joins {
        base.insert(m.id.clone(), m.clone());
    }
}

impl State {
    /// Sends `diff` to every viewer; `from_sync` (a snapshot answer) goes into the waiting
    /// viewers' starting members instead of their queue.
    fn broadcast(&mut self, diff: &Diff, from_sync: bool) {
        if diff.is_empty() {
            return;
        }
        self.viewers.retain(|_, v| {
            match &mut v.pending {
                Some(p) if from_sync => apply_to_base(&mut p.base, diff),
                Some(p) => p.queue.push(diff.clone()),
                None => return v.tx.send(HubEvent::Diff(diff.clone())).is_ok(),
            }
            !v.tx.is_closed()
        });
    }
}

impl HubInner {
    fn msg(&self, event: PresenceEvent) -> PresenceMsg {
        PresenceMsg {
            scope: self.scope.clone(),
            channel: self.channel.clone(),
            instance: self.instance.clone(),
            event,
        }
    }

    /// A member message from another instance.
    fn on_remote(&self, payload: &[u8], from_sync: bool) {
        let Ok(msg) = serde_json::from_slice::<PresenceMsg>(payload) else {
            tracing::debug!(channel_hash = %self.key, "a presence message that isn't one");
            return;
        };
        if msg.instance == self.instance {
            return;
        }
        let mut state = self.state.lock().unwrap();
        match state.presence.apply(&msg, Instant::now()) {
            Ok(diff) => state.broadcast(&diff, from_sync),
            Err(PresenceError::ScopeMismatch) => {
                tracing::warn!(channel_hash = %self.key, "a presence message for another scope (R4): dropped");
            }
            Err(e) => {
                tracing::debug!(channel_hash = %self.key, error = %e, "presence message refused")
            }
        }
    }

    /// Another instance asks for this instance's members.
    async fn on_sync_request(&self, reply: Option<String>, payload: &[u8]) {
        let Some(reply) = reply else { return };
        let Ok(req) = serde_json::from_slice::<SyncRequest>(payload) else {
            return;
        };
        if req.instance == self.instance {
            return;
        }
        let members = self
            .state
            .lock()
            .unwrap()
            .presence
            .members_of(&self.instance);
        let answer = self.msg(PresenceEvent::Snapshot { members });
        self.publish_to(&reply, &answer).await;
    }

    fn finish_sync(&self) {
        let mut state = self.state.lock().unwrap();
        state.synced = true;
        state.viewers.retain(|_, v| {
            if let Some(p) = v.pending.take() {
                let mut ok =
                    v.tx.send(HubEvent::State(p.base.into_values().collect()))
                        .is_ok();
                for d in p.queue {
                    ok = ok && v.tx.send(HubEvent::Diff(d)).is_ok();
                }
                ok
            } else {
                !v.tx.is_closed()
            }
        });
    }

    async fn publish_to(&self, subject: &str, msg: &PresenceMsg) {
        let payload = Bytes::from(serde_json::to_vec(msg).expect("presence serializes"));
        if let Err(e) = self.bus.publish(subject, None, payload).await {
            tracing::warn!(channel_hash = %self.key, error = %e, "presence publish failed");
        }
    }

    async fn subscribe(&self, subject: &str) -> BusStream {
        match self.bus.subscribe(subject).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(channel_hash = %self.key, error = %e, "presence subscribe failed: local members only");
                Box::pin(futures_util::stream::pending())
            }
        }
    }

    /// Asks every other instance for its members (answers on `inbox`).
    async fn sync_request(&self, inbox: &str) {
        let request = serde_json::to_vec(&SyncRequest {
            instance: self.instance.clone(),
        })
        .expect("a sync request serializes");
        if let Err(e) = self
            .bus
            .publish(&sync_subject(&self.key), Some(inbox), request.into())
            .await
        {
            tracing::warn!(channel_hash = %self.key, error = %e, "presence sync request failed");
        }
    }

    async fn run(self: Arc<Self>, sync_wait: Duration, mut resync: mpsc::UnboundedReceiver<()>) {
        let mut members = self.subscribe(&presence_subject(&self.key)).await;
        let mut requests = self.subscribe(&sync_subject(&self.key)).await;
        let inbox = self.bus.new_inbox();
        // Kept for the hub's life: a lost instance's members come back on it too.
        let mut answers = self.subscribe(&inbox).await;
        self.sync_request(&inbox).await;
        let sync_end = tokio::time::sleep(sync_wait);
        tokio::pin!(sync_end);
        let mut syncing = true;
        // A subscription that ends (the bus closed it) is replaced by one that never yields.
        let ended = || -> BusStream { Box::pin(futures_util::stream::pending()) };
        loop {
            tokio::select! {
                m = members.next() => match m {
                    Some(m) => self.on_remote(&m.payload, false),
                    None => members = ended(),
                },
                m = requests.next() => match m {
                    Some(m) => self.on_sync_request(m.reply, &m.payload).await,
                    None => requests = ended(),
                },
                // During the start-up sync the answers are the waiting viewers' starting
                // members; afterwards (a lost instance back) they are changes like any other.
                m = answers.next() => match m {
                    Some(m) => self.on_remote(&m.payload, syncing),
                    None => answers = ended(),
                },
                Some(()) = resync.recv() => self.sync_request(&inbox).await,
                _ = &mut sync_end, if syncing => {
                    syncing = false;
                    self.finish_sync();
                }
            }
        }
    }
}

impl Hub {
    /// A hub for `channel` under `scope` (its hash `key`), starting its bus traffic and its
    /// sync request.
    pub(crate) fn start(
        scope: Scope,
        channel: &str,
        key: &str,
        instance: &str,
        bus: Arc<dyn Bus>,
        max_members: usize,
        sync_wait: Duration,
    ) -> Self {
        let (resync, resync_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(HubInner {
            scope: scope.clone(),
            channel: channel.to_string(),
            key: key.to_string(),
            instance: instance.to_string(),
            bus,
            state: Mutex::new(State {
                presence: ChannelPresence::new(scope, channel, max_members),
                viewers: HashMap::new(),
                synced: false,
                lost: HashSet::new(),
            }),
            resync,
        });
        let task = tokio::spawn(inner.clone().run(sync_wait, resync_rx)).abort_handle();
        Self { inner, task }
    }

    /// Registers a local viewer whose own member is `member`: the member's join message to
    /// publish, and the viewer's events.
    pub(crate) fn add_viewer(
        &self,
        viewer: u64,
        member: &Member,
    ) -> Result<(mpsc::UnboundedReceiver<HubEvent>, PresenceMsg), PresenceError> {
        let msg = self.inner.msg(PresenceEvent::Join {
            member: member.clone(),
        });
        let mut state = self.inner.state.lock().unwrap();
        let diff = state.presence.apply(&msg, Instant::now())?;
        state.broadcast(&diff, false);
        let (tx, rx) = mpsc::unbounded_channel();
        let pending = if state.synced {
            let _ = tx.send(HubEvent::State(state.presence.members()));
            None
        } else {
            Some(Pending {
                base: state
                    .presence
                    .members()
                    .into_iter()
                    .map(|m| (m.id.clone(), m))
                    .collect(),
                queue: vec![],
            })
        };
        state.viewers.insert(viewer, Viewer { tx, pending });
        Ok((rx, msg))
    }

    /// A local member's new state: the message to publish (`None` when nothing changed).
    pub(crate) fn update_local(&self, member: &Member) -> Option<PresenceMsg> {
        let msg = self.inner.msg(PresenceEvent::Join {
            member: member.clone(),
        });
        let mut state = self.inner.state.lock().unwrap();
        match state.presence.apply(&msg, Instant::now()) {
            Ok(diff) if diff.is_empty() => None,
            Ok(diff) => {
                state.broadcast(&diff, false);
                Some(msg)
            }
            Err(_) => None,
        }
    }

    /// Takes a viewer and its member out: the leave message to publish, and how many viewers
    /// are left.
    pub(crate) fn remove_viewer(&self, viewer: u64, member_id: &str) -> (PresenceMsg, usize) {
        let msg = self.inner.msg(PresenceEvent::Leave {
            id: member_id.to_string(),
        });
        let mut state = self.inner.state.lock().unwrap();
        state.viewers.remove(&viewer);
        if let Ok(diff) = state.presence.apply(&msg, Instant::now()) {
            state.broadcast(&diff, false);
        }
        (msg, state.viewers.len())
    }

    pub(crate) fn viewers(&self) -> usize {
        self.inner.state.lock().unwrap().viewers.len()
    }

    /// Another instance's heartbeat; one whose members expired here is asked for them again.
    pub(crate) fn heartbeat(&self, instance: &str, now: Instant) {
        let mut state = self.inner.state.lock().unwrap();
        state.presence.heartbeat(instance, now);
        if state.lost.remove(instance) {
            let _ = self.inner.resync.send(());
        }
    }

    /// This instance is alive; instances silent for longer than `after` lose their members.
    pub(crate) fn tick(&self, now: Instant, after: Duration) {
        let mut state = self.inner.state.lock().unwrap();
        let instance = self.inner.instance.clone();
        state.presence.heartbeat(&instance, now);
        let (diff, dead) = state.presence.expire_instances(now, after);
        state.lost.extend(dead);
        state.broadcast(&diff, false);
    }

    /// Publishes one of this hub's messages on the channel's subject.
    pub(crate) async fn publish(&self, msg: &PresenceMsg) {
        self.inner
            .publish_to(&presence_subject(&self.inner.key), msg)
            .await;
    }
}
