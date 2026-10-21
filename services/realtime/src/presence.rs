//! Presence (spec §5.1, §5.2, R4, R15, R20).
//!
//! - Each instance owns its sockets' members and publishes joins, leaves and updates on
//!   `chelabase.rt.presence.<channel_hash>` ([`PresenceMsg`], carrying its full R4 scope), and a
//!   heartbeat every 5 s on `chelabase.rt.presence.hb.<instance_id>`. An instance that starts
//!   watching a channel asks `chelabase.rt.presence.sync.<channel_hash>` for snapshots (every
//!   instance answers with its local members on the request's reply inbox).
//! - A receiver drops a message whose scope or channel isn't its channel's (R4). Three missed
//!   heartbeats drop that instance's members; a closed socket leaves at once.
//! - A member's `state.row` is checked as the setter first (`ChangeSource::visible`; not
//!   visible or an error → `error invalid`, R15); each viewer gets the state blanked when its
//!   own check identity can't see the row; a `rows` batch touching a selected key re-checks.
//! - State at most 1 KB; at most 1000 members per channel; 20 presence frames per second per
//!   socket, the latest state winning; a degraded channel gets no presence (R20).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use chelabase_nats::rt::PRESENCE;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::channel::Scope;
use crate::protocol::{Member, TableState};

/// The largest presence state, as JSON.
pub const MAX_STATE_BYTES: usize = 1024;

/// `chelabase.rt.presence.<hash>`.
pub fn presence_subject(channel_hash: &str) -> String {
    format!("{PRESENCE}.{channel_hash}")
}
/// `chelabase.rt.presence.hb.<instance_id>`.
pub fn heartbeat_subject(instance_id: &str) -> String {
    format!("{PRESENCE}.hb.{instance_id}")
}
/// `chelabase.rt.presence.sync.<hash>`.
pub fn sync_subject(channel_hash: &str) -> String {
    format!("{PRESENCE}.sync.{channel_hash}")
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PresenceError {
    #[error("the message is for another scope or channel")]
    ScopeMismatch,
    #[error("the channel has its maximum of members")]
    Full,
    #[error("the state is over 1 KB")]
    TooLarge,
    #[error("invalid state: {0}")]
    Invalid(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TableStateIn {
    row: Option<String>,
    column: Option<String>,
    editing: bool,
}

/// A table member's state from a `presence` frame: `{row: string|null, column: string|null,
/// editing: bool}`, unknown fields refused, at most [`MAX_STATE_BYTES`].
pub fn parse_table_state(state: &Value) -> Result<TableState, PresenceError> {
    let size = serde_json::to_string(state)
        .map(|s| s.len())
        .unwrap_or(usize::MAX);
    if size > MAX_STATE_BYTES {
        return Err(PresenceError::TooLarge);
    }
    let s: TableStateIn =
        serde_json::from_value(state.clone()).map_err(|e| PresenceError::Invalid(e.to_string()))?;
    Ok(TableState {
        row: s.row,
        column: s.column,
        editing: s.editing,
    })
}

/// The member as a viewer that can't see its row gets it: every field kept, the state blanked.
pub fn blank(member: &Member) -> Member {
    Member {
        state: TableState::default(),
        ..member.clone()
    }
}

/// A presence message between instances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresenceMsg {
    pub scope: Scope,
    pub channel: String,
    pub instance: String,
    pub event: PresenceEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PresenceEvent {
    /// A new member, or a member's new state.
    Join {
        member: Member,
    },
    Leave {
        id: String,
    },
    /// Every member the instance holds for the channel (an answer to a sync request).
    Snapshot {
        members: Vec<Member>,
    },
}

/// What changed for the channel's viewers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    pub joins: Vec<Member>,
    pub leaves: Vec<Member>,
}

impl Diff {
    pub fn is_empty(&self) -> bool {
        self.joins.is_empty() && self.leaves.is_empty()
    }
}

/// One channel's members across instances.
#[derive(Debug)]
pub struct ChannelPresence {
    scope: Scope,
    channel: String,
    max_members: usize,
    /// Member id → (the instance holding it, the member).
    members: BTreeMap<String, (String, Member)>,
    /// Instance → when it was last heard from (a heartbeat or a message).
    last_seen: HashMap<String, Instant>,
}

impl ChannelPresence {
    pub fn new(scope: Scope, channel: impl Into<String>, max_members: usize) -> Self {
        Self {
            scope,
            channel: channel.into(),
            max_members,
            members: BTreeMap::new(),
            last_seen: HashMap::new(),
        }
    }

    fn seen(&mut self, instance: &str, now: Instant) {
        let at = self.last_seen.entry(instance.to_string()).or_insert(now);
        if *at < now {
            *at = now;
        }
    }

    /// Puts `member` (held by `instance`) in, as a diff.
    fn put(
        &mut self,
        instance: &str,
        member: &Member,
        diff: &mut Diff,
    ) -> Result<(), PresenceError> {
        let full = self.members.len() >= self.max_members;
        match self.members.get_mut(&member.id) {
            Some((_, old)) if old == member => Ok(()),
            Some((holder, old)) => {
                diff.leaves.push(old.clone());
                diff.joins.push(member.clone());
                *holder = instance.to_string();
                *old = member.clone();
                Ok(())
            }
            None if full => Err(PresenceError::Full),
            None => {
                diff.joins.push(member.clone());
                self.members
                    .insert(member.id.clone(), (instance.to_string(), member.clone()));
                Ok(())
            }
        }
    }

    /// Applies a message. A join of a known id with another state is a leave plus a join; the
    /// same state is no change; a snapshot replaces that instance's members. A new member past
    /// the maximum is [`PresenceError::Full`].
    pub fn apply(&mut self, msg: &PresenceMsg, now: Instant) -> Result<Diff, PresenceError> {
        if msg.scope != self.scope || msg.channel != self.channel {
            return Err(PresenceError::ScopeMismatch);
        }
        self.seen(&msg.instance, now);
        let mut diff = Diff::default();
        match &msg.event {
            PresenceEvent::Join { member } => self.put(&msg.instance, member, &mut diff)?,
            PresenceEvent::Leave { id } => {
                // Only the instance holding a member may take it out.
                if self
                    .members
                    .get(id)
                    .is_some_and(|(i, _)| *i == msg.instance)
                    && let Some((_, m)) = self.members.remove(id)
                {
                    diff.leaves.push(m);
                }
            }
            PresenceEvent::Snapshot { members } => {
                let keep: std::collections::HashSet<&str> =
                    members.iter().map(|m| m.id.as_str()).collect();
                let gone: Vec<String> = self
                    .members
                    .iter()
                    .filter(|(id, (i, _))| *i == msg.instance && !keep.contains(id.as_str()))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in gone {
                    if let Some((_, m)) = self.members.remove(&id) {
                        diff.leaves.push(m);
                    }
                }
                for m in members {
                    // A full channel takes the snapshot's members up to its maximum.
                    let _ = self.put(&msg.instance, m, &mut diff);
                }
            }
        }
        Ok(diff)
    }
    pub fn heartbeat(&mut self, instance: &str, now: Instant) {
        self.seen(instance, now);
    }
    /// Drops the members of every instance whose last heartbeat is older than `after`.
    pub fn expire(&mut self, now: Instant, after: Duration) -> Diff {
        self.expire_instances(now, after).0
    }
    /// [`ChannelPresence::expire`], and the instances it dropped.
    pub fn expire_instances(&mut self, now: Instant, after: Duration) -> (Diff, Vec<String>) {
        let dead: Vec<String> = self
            .last_seen
            .iter()
            .filter(|(_, at)| now.saturating_duration_since(**at) > after)
            .map(|(i, _)| i.clone())
            .collect();
        let mut diff = Diff::default();
        for instance in &dead {
            let instance = instance.clone();
            self.last_seen.remove(&instance);
            let ids: Vec<String> = self
                .members
                .iter()
                .filter(|(_, (i, _))| *i == instance)
                .map(|(id, _)| id.clone())
                .collect();
            for id in ids {
                if let Some((_, m)) = self.members.remove(&id) {
                    diff.leaves.push(m);
                }
            }
        }
        (diff, dead)
    }
    /// Every member, sorted by id.
    pub fn members(&self) -> Vec<Member> {
        self.members.values().map(|(_, m)| m.clone()).collect()
    }
    /// The members `instance` holds, sorted by id.
    pub fn members_of(&self, instance: &str) -> Vec<Member> {
        self.members
            .values()
            .filter(|(i, _)| i == instance)
            .map(|(_, m)| m.clone())
            .collect()
    }
    pub fn len(&self) -> usize {
        self.members.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// R15: 20 presence frames per second per socket; over that a frame is held and the latest
/// held state goes out when a slot frees (the others are dropped).
#[derive(Debug)]
pub struct PresenceThrottle {
    per_sec: u32,
    sent: VecDeque<Instant>,
    pending: Option<TableState>,
}

impl PresenceThrottle {
    pub fn new(per_sec: u32) -> Self {
        Self {
            per_sec,
            sent: VecDeque::new(),
            pending: None,
        }
    }
    fn prune(&mut self, now: Instant) {
        while self
            .sent
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= Duration::from_secs(1))
        {
            self.sent.pop_front();
        }
    }
    fn take_slot(&mut self, now: Instant) -> bool {
        self.prune(now);
        if self.sent.len() < self.per_sec as usize {
            self.sent.push_back(now);
            true
        } else {
            false
        }
    }
    /// `Some(state)` to send now, `None` when it is held.
    pub fn offer(&mut self, state: TableState, now: Instant) -> Option<TableState> {
        if self.take_slot(now) {
            // A newer state supersedes any held one.
            self.pending = None;
            Some(state)
        } else {
            self.pending = Some(state);
            None
        }
    }
    /// The latest held state, once a slot is free (then nothing is held).
    pub fn take_pending(&mut self, now: Instant) -> Option<TableState> {
        if self.pending.is_none() || !self.take_slot(now) {
            return None;
        }
        self.pending.take()
    }
    /// When a held state can go out (the oldest slot frees), if one is held.
    pub fn next_slot(&self) -> Option<Instant> {
        self.pending.as_ref()?;
        self.sent.front().map(|t| *t + Duration::from_secs(1))
    }
}
