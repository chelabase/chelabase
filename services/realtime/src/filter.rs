//! Per-role filtering of one watched relation on one instance (spec §4; R2, R12, R15, R19, W2,
//! W6, W26, S30). Each instance filters for its own viewers (P8).
//!
//! Per window ([`TableFilter::deliver`]), per check group (all groups concurrently, each under
//! the database's [`crate::check::Limiter`]):
//!
//! - `changed` (a `c`, over the key limit, a non-keyed fan-out, an unsupported key type or key
//!   text) → every viewer gets `{op: "changed"}`; no check.
//! - Every group is checked in every window (a probe-only check when the window has no inserts
//!   or updates): inserts and updates go with the window's xids (polled until visible, P7);
//!   only the keys the check returned are delivered. The probe in the same session reclassifies
//!   the group (R2) before anything is released; a failed probe, an error, a timeout, an
//!   overflow or a never-visible xid → that group's viewers get `changed`.
//! - Deletes (converted to key strings in the group's session): a bypass group's viewers get
//!   them keyed; an RLS group's viewer gets a keyed delete only for keys in its view set
//!   ([`TableFilter::view`], R19: only keys a check confirmed visible) or delivered to it
//!   before; other deletes send nothing.
//!   A key that can't be converted is a keyless delete for bypass viewers.
//! - W26: an RLS group's update key the check didn't return but which is in a viewer's view set
//!   is sent to that viewer as a keyed delete.
//! - One `rows` event per viewer per window: `insert`, `update`, `delete` (keyed), `delete`
//!   (keyless), `changed`, in that order, empty ones left out; nothing at all when empty.
//! - Sinks are written with `try_send`, never awaited (T7's audit rule). A full sink drops that
//!   viewer's event; the next delivery to it sends `resync {reason: "overflow"}` instead of that
//!   window's rows (the refetch covers them), until a resync gets through.
//! - S30: windows are delivered in order; one still unresolved [`crate::check::WINDOW_DEADLINE`]
//!   after it closed is sent as keyless `changed`.
//! - The R12 cache is flushed on every window (the source flushes it for the whole database).
//!
//! Object storage m17: a filter with a bucket ([`TableFilter::set_bucket`], a storage
//! channel's `storage.objects`) scopes every check of every group to that bucket, so a key is
//! released only for an object of that bucket the group's own read sees. Its groups are never
//! bypass groups: a deleted row has no bucket left to check, so even owner mode gets a keyed
//! delete only for a key on its screen or delivered to it, and never a keyless one.
//!
//! The inserts' and updates' plugin texts are mapped to the group's key strings by converting
//! them in the group's own session (as deletes are), in the check's own transaction
//! (`CheckRequest::convert`): the check answers the session's key strings of the rows it sees,
//! and only a key whose conversion is in that answer is released.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chelabase_query::realtime::{Relation, key_string, key_text_allowed, keys_supported};
use futures_util::future::join_all;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::time::Instant;

use crate::check::{
    CONCURRENCY, CheckAnswer, CheckRequest, CheckTarget, Checker, GroupKey, Limiter,
    VISIBILITY_POLL, VISIBILITY_WAIT, VisibilityCache, WINDOW_DEADLINE, group_key,
};
use crate::merge::{MAX_KEYS, RelWindow};
use crate::protocol::{Change, ChangeOp};
use crate::source::{ChannelEvent, CheckIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterSettings {
    pub concurrency: usize,
    pub visibility_wait: Duration,
    pub visibility_poll: Duration,
    pub window_deadline: Duration,
    pub max_keys: usize,
}

impl Default for FilterSettings {
    /// 8, 2 s, 50 ms, 5 s, 1000.
    fn default() -> Self {
        Self {
            concurrency: CONCURRENCY,
            visibility_wait: VISIBILITY_WAIT,
            visibility_poll: VISIBILITY_POLL,
            window_deadline: WINDOW_DEADLINE,
            max_keys: MAX_KEYS,
        }
    }
}

struct Viewer {
    group: GroupKey,
    /// Its own identity (its latest verified token): the group's when it's the newest.
    identity: CheckIdentity,
    sink: mpsc::Sender<ChannelEvent>,
    /// R19: the keys on its screen.
    view: HashSet<String>,
    /// Keys delivered to it (inserted or updated) and not deleted since.
    delivered: HashSet<String>,
    /// An event was dropped on a full sink (here, or a status the source dropped): the next
    /// delivery is a resync.
    overflowed: Arc<AtomicBool>,
}

/// One visibility check of a viewer's keys, made under the filter and run without it
/// ([`TableFilter::ask_keys`], [`KeyCheck::run`]).
pub struct KeyCheck {
    checker: Arc<dyn Checker>,
    limiter: Arc<Limiter>,
    cache: Arc<VisibilityCache>,
    target: CheckTarget,
    identity: CheckIdentity,
    group: GroupKey,
    rel_oid: u32,
    keys: Vec<String>,
    /// Each key's answer known already (cached, or a key the relation can't take).
    known: Vec<Option<bool>>,
    /// The keys to check: (index, texts).
    ask: Vec<(usize, Vec<String>)>,
    /// The R12 cache's generation when the check was made.
    generation: u64,
    /// The filter's bucket (a storage channel's).
    bucket: Option<uuid::Uuid>,
}

impl KeyCheck {
    /// The answers (and the R12 cache written); `Err`: no permit, an error, a failed probe.
    #[allow(clippy::result_unit_err)]
    pub async fn run(&self) -> Result<Vec<bool>, ()> {
        let mut out = self.known.clone();
        if !self.ask.is_empty() {
            let req = CheckRequest {
                rel_oid: self.rel_oid,
                xids: Vec::new(),
                keys: Some(self.ask.iter().map(|(_, t)| t.clone()).collect()),
                convert: Vec::new(),
                bucket: self.bucket,
            };
            let permit = self.limiter.try_permit().ok_or(())?;
            let answer = self
                .checker
                .check(&self.target, &self.identity, &req)
                .await
                .map_err(|_| ())?;
            drop(permit);
            if !answer.probe_ok || !answer.visible {
                return Err(());
            }
            let seen: HashSet<&String> = answer.keys.iter().collect();
            let at = Instant::now();
            for (i, _) in &self.ask {
                let visible = seen.contains(&self.keys[*i]);
                self.cache.put_since(
                    self.generation,
                    at,
                    &self.group,
                    self.rel_oid,
                    &self.keys[*i],
                    visible,
                );
                out[*i] = Some(visible);
            }
        }
        Ok(out.into_iter().map(|v| v.unwrap_or(false)).collect())
    }
}

struct Group {
    /// Who the group is checked as (any of its viewers: the role is the group).
    identity: CheckIdentity,
    bypass: bool,
    viewers: Vec<u64>,
}

/// What one group's check of a window came to.
enum Outcome {
    Changed,
    Checked(Checked),
}

struct Checked {
    bypass: bool,
    inserts: Vec<String>,
    updates: Vec<String>,
    /// Updates the check didn't return (hidden from the group, or gone).
    hidden_updates: Vec<String>,
    /// Each delete's key string (`None`: it couldn't be converted).
    deletes: Vec<Option<String>>,
}

/// One relation's viewers on this instance.
pub struct TableFilter {
    target: CheckTarget,
    relation: Relation,
    checker: Arc<dyn Checker>,
    limiter: Arc<Limiter>,
    cache: Arc<VisibilityCache>,
    settings: FilterSettings,
    viewers: HashMap<u64, Viewer>,
    /// In first-joined order (deterministic check order).
    groups: Vec<(GroupKey, Group)>,
    /// A legacy inheritance parent (R16): every window is `changed`.
    changed_only: bool,
    /// Object storage m17: a storage channel's bucket (see the module docs).
    bucket: Option<uuid::Uuid>,
}

impl TableFilter {
    pub fn new(
        target: CheckTarget,
        relation: Relation,
        checker: Arc<dyn Checker>,
        limiter: Arc<Limiter>,
        cache: Arc<VisibilityCache>,
        settings: FilterSettings,
    ) -> Self {
        Self {
            target,
            relation,
            checker,
            limiter,
            cache,
            settings,
            viewers: HashMap::new(),
            groups: Vec::new(),
            changed_only: false,
            bucket: None,
        }
    }
    /// Object storage m17: scopes every check to `bucket` (a storage channel's filter of
    /// `storage.objects`; set before any viewer is added).
    pub fn set_bucket(&mut self, bucket: Option<uuid::Uuid>) {
        self.bucket = bucket;
    }
    /// The bucket every check is scoped to.
    pub fn bucket(&self) -> Option<uuid::Uuid> {
        self.bucket
    }
    /// The relation the filter checks against.
    pub fn relation(&self) -> &Relation {
        &self.relation
    }
    /// A re-resolved relation (W10: a recreated table, a changed key): the groups keep their
    /// viewers; what was delivered under the old relation no longer counts.
    pub fn set_relation(&mut self, relation: Relation) {
        if relation != self.relation {
            for v in self.viewers.values_mut() {
                v.delivered.clear();
            }
        }
        self.relation = relation;
    }
    /// Every window `changed` (a legacy inheritance parent, R16).
    pub fn set_changed_only(&mut self, on: bool) {
        self.changed_only = on;
    }
    pub fn is_empty(&self) -> bool {
        self.viewers.is_empty()
    }
    fn group(&self, key: &GroupKey) -> Option<&Group> {
        self.groups.iter().find(|(k, _)| k == key).map(|(_, g)| g)
    }
    fn group_mut(&mut self, key: &GroupKey) -> Option<&mut Group> {
        self.groups
            .iter_mut()
            .find(|(k, _)| k == key)
            .map(|(_, g)| g)
    }
    /// A viewer, classified by its join probe (`bypass`).
    pub fn add(
        &mut self,
        viewer: u64,
        identity: CheckIdentity,
        bypass: bool,
        sink: mpsc::Sender<ChannelEvent>,
    ) {
        self.add_shared(viewer, identity, bypass, sink, Arc::default());
    }
    /// [`Self::add`] with the viewer's overflow flag shared with whoever else writes its sink
    /// (the source's statuses): set there, the next delivery here is a resync.
    pub fn add_shared(
        &mut self,
        viewer: u64,
        identity: CheckIdentity,
        bypass: bool,
        sink: mpsc::Sender<ChannelEvent>,
        overflowed: Arc<AtomicBool>,
    ) {
        self.remove(viewer);
        // A bucket's groups are never bypass groups (see the module docs).
        let bypass = bypass && self.bucket.is_none();
        let key = group_key(&identity);
        match self.group_mut(&key) {
            Some(g) => {
                // The newest join's probe is the freshest classification, and its identity
                // (an owner's token) the most recently verified one.
                g.bypass = bypass;
                g.identity = identity.clone();
                g.viewers.push(viewer);
            }
            None => self.groups.push((
                key.clone(),
                Group {
                    identity: identity.clone(),
                    bypass,
                    viewers: vec![viewer],
                },
            )),
        }
        self.viewers.insert(
            viewer,
            Viewer {
                group: key,
                identity,
                sink,
                view: HashSet::new(),
                delivered: HashSet::new(),
                overflowed,
            },
        );
    }
    pub fn remove(&mut self, viewer: u64) {
        let Some(v) = self.viewers.remove(&viewer) else {
            return;
        };
        if let Some(pos) = self.groups.iter().position(|(k, _)| *k == v.group) {
            let g = &mut self.groups[pos].1;
            g.viewers.retain(|id| *id != viewer);
            match g.viewers.last() {
                None => {
                    self.groups.remove(pos);
                }
                // The group is checked as a viewer that still watches (an owner group's
                // `/visible` calls never carry the token of a member who left).
                Some(newest) => {
                    if let Some(n) = self.viewers.get(newest) {
                        g.identity = n.identity.clone();
                    }
                }
            }
        }
    }
    /// R19 as ruled: the client's `view` keys; only those the viewer's group is confirmed to
    /// see now (one visibility check per call, read fresh and written to the R12 cache; a failed
    /// check confirms nothing) replace its view set, so a guessed hidden key never earns a keyed delete. Keys
    /// delivered to it as visible still count for deletes.
    pub async fn view(&mut self, viewer: u64, keys: Vec<String>) {
        let Some(ask) = self.ask_keys(viewer, &keys, false) else {
            self.clear_view(viewer);
            return;
        };
        let seen = ask.run().await;
        self.set_view(viewer, &ask, keys, seen);
    }
    /// No key of the viewer's screen is confirmed (its keys can't be checked).
    pub fn clear_view(&mut self, viewer: u64) {
        if let Some(v) = self.viewers.get_mut(&viewer) {
            v.view.clear();
        }
    }
    /// The viewer's view set from a check `ask` made for it (`seen`: its answers): only the
    /// keys it confirmed visible. Nothing when the viewer left or the relation changed since.
    #[allow(clippy::result_unit_err)]
    pub fn set_view(&mut self, viewer: u64, ask: &KeyCheck, keys: Vec<String>, seen: Result<Vec<bool>, ()>) {
        if ask.rel_oid != self.relation.oid {
            return;
        }
        let confirmed: HashSet<String> = match seen {
            Ok(seen) => keys
                .into_iter()
                .zip(seen)
                .filter_map(|(k, visible)| visible.then_some(k))
                .collect(),
            Err(()) => HashSet::new(),
        };
        if let Some(v) = self.viewers.get_mut(&viewer) {
            v.view = confirmed;
        }
    }
    /// A verified token renewal (T8): the viewer's group is checked with `token` from now on
    /// (the owner path's `/visible` calls carry it; a role's checks don't use it).
    pub fn renew(&mut self, viewer: u64, token: Arc<str>) {
        let Some(v) = self.viewers.get_mut(&viewer) else {
            return;
        };
        v.identity.token = token;
        let (key, identity) = (v.group.clone(), v.identity.clone());
        if let Some(g) = self.group_mut(&key) {
            g.identity = identity;
        }
    }
    /// The viewer's group's current classification.
    pub fn is_bypass(&self, viewer: u64) -> Option<bool> {
        let v = self.viewers.get(&viewer)?;
        self.group(&v.group).map(|g| g.bypass)
    }

    /// Whether the window's keys can be checked at all (P6): the relation's key types and every
    /// key text, with one text per key column.
    fn checkable(&self, window: &RelWindow) -> bool {
        if !keys_supported(&self.relation) {
            return false;
        }
        let key = &self.relation.key;
        window
            .inserts
            .iter()
            .chain(&window.updates)
            .chain(&window.deletes)
            .all(|texts| {
                texts.len() == key.len()
                    && texts.iter().zip(key).all(|(t, c)| key_text_allowed(c, t))
            })
    }

    /// Checks one window for every group and sends each viewer its event. `xids` are the
    /// window's; `closed_at` is when the window closed (S30's deadline counts from it).
    pub async fn deliver(&mut self, window: &RelWindow, xids: &[u64], closed_at: Instant) {
        self.cache.flush();
        if self.viewers.is_empty() {
            return;
        }
        let empty =
            window.inserts.is_empty() && window.updates.is_empty() && window.deletes.is_empty();
        if window.changed || self.changed_only || !self.checkable(window) {
            self.send_all_changed();
            return;
        }
        if empty {
            return;
        }
        let deadline = closed_at + self.settings.window_deadline;
        let outcomes: Vec<Outcome> = {
            let this = &*self;
            let checks = this.groups.iter().map(|(_, g)| {
                let identity = g.identity.clone();
                async move {
                    match tokio::time::timeout_at(
                        deadline,
                        this.check_group(&identity, window, xids),
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(_) => Outcome::Changed,
                    }
                }
            });
            join_all(checks).await
        };
        let groups: Vec<(GroupKey, Vec<u64>)> = self
            .groups
            .iter()
            .map(|(k, g)| (k.clone(), g.viewers.clone()))
            .collect();
        for ((key, viewers), outcome) in groups.into_iter().zip(outcomes) {
            match outcome {
                Outcome::Changed => {
                    for id in viewers {
                        self.send(id, vec![changed()]);
                    }
                }
                Outcome::Checked(c) => {
                    if let Some(g) = self.group_mut(&key) {
                        g.bypass = c.bypass;
                    }
                    for id in viewers {
                        let changes = self.compose(id, &c);
                        self.send(id, changes);
                    }
                }
            }
        }
        self.cache.flush();
    }

    /// One viewer's changes of a checked window (and its delivered set updated).
    fn compose(&mut self, id: u64, c: &Checked) -> Vec<Change> {
        let Some(v) = self.viewers.get_mut(&id) else {
            return Vec::new();
        };
        let mut keyed_deletes: Vec<String> = Vec::new();
        let mut keyless = false;
        let push_delete = |k: &String, out: &mut Vec<String>| {
            if !out.contains(k) {
                out.push(k.clone());
            }
        };
        if c.bypass {
            for d in &c.deletes {
                match d {
                    Some(k) => push_delete(k, &mut keyed_deletes),
                    None => keyless = true,
                }
            }
        } else {
            for k in c.deletes.iter().flatten() {
                if v.view.contains(k) || v.delivered.contains(k) {
                    push_delete(k, &mut keyed_deletes);
                }
            }
            // W26: a row on its screen that its read no longer returns.
            for k in &c.hidden_updates {
                if v.view.contains(k) {
                    push_delete(k, &mut keyed_deletes);
                }
            }
        }
        for k in c.inserts.iter().chain(&c.updates) {
            v.delivered.insert(k.clone());
        }
        for k in &keyed_deletes {
            v.delivered.remove(k);
        }
        let mut changes = Vec::new();
        if !c.inserts.is_empty() {
            changes.push(Change {
                op: ChangeOp::Insert,
                keys: Some(c.inserts.clone()),
            });
        }
        if !c.updates.is_empty() {
            changes.push(Change {
                op: ChangeOp::Update,
                keys: Some(c.updates.clone()),
            });
        }
        if !keyed_deletes.is_empty() {
            changes.push(Change {
                op: ChangeOp::Delete,
                keys: Some(keyed_deletes),
            });
        }
        if keyless {
            changes.push(Change {
                op: ChangeOp::Delete,
                keys: None,
            });
        }
        changes
    }

    fn send_all_changed(&mut self) {
        let ids: Vec<u64> = self.viewers.keys().copied().collect();
        for id in ids {
            self.send(id, vec![changed()]);
        }
    }

    /// `try_send`, never awaited: a full sink drops the event and owes the viewer a resync.
    fn send(&mut self, id: u64, changes: Vec<Change>) {
        let Some(v) = self.viewers.get_mut(&id) else {
            return;
        };
        if v.overflowed.load(Ordering::SeqCst) {
            match v.sink.try_send(ChannelEvent::Resync {
                reason: "overflow".into(),
            }) {
                Ok(()) => v.overflowed.store(false, Ordering::SeqCst),
                Err(TrySendError::Full(_)) | Err(TrySendError::Closed(_)) => {}
            }
            return;
        }
        if changes.is_empty() {
            return;
        }
        if let Err(TrySendError::Full(_)) = v.sink.try_send(ChannelEvent::Rows(changes)) {
            v.overflowed.store(true, Ordering::SeqCst);
        }
    }

    /// One group's check of a window: the probe, the visibility of `xids` (polled, P7) and the
    /// visible keys, then its session's key strings of every key of the window.
    async fn check_group(&self, id: &CheckIdentity, window: &RelWindow, xids: &[u64]) -> Outcome {
        let read: Vec<Vec<String>> = window
            .inserts
            .iter()
            .chain(&window.updates)
            .cloned()
            .collect();
        // Every key of the window, converted to the group's session form in the check's own
        // session and transaction.
        let all: Vec<Vec<String>> = window
            .inserts
            .iter()
            .chain(&window.updates)
            .chain(&window.deletes)
            .cloned()
            .collect();
        let req = if read.is_empty() {
            // Deletes only: no row to read, so no visibility to wait for; the probe still
            // reclassifies the group before anything is released.
            CheckRequest {
                rel_oid: self.relation.oid,
                xids: Vec::new(),
                keys: None,
                convert: all,
                bucket: self.bucket,
            }
        } else {
            CheckRequest {
                rel_oid: self.relation.oid,
                xids: xids.to_vec(),
                keys: Some(read),
                convert: all,
                bucket: self.bucket,
            }
        };
        let Some(answer) = self.poll(id, &req).await else {
            return Outcome::Changed;
        };
        let converted = answer.converted;
        if converted.len() != req.convert.len() {
            // Without the session's form no read key can be matched: refetch.
            return Outcome::Changed;
        }
        let (read_keys, deletes) = converted.split_at(window.inserts.len() + window.updates.len());
        if read_keys.iter().any(Option::is_none) {
            return Outcome::Changed;
        }
        let visible: HashSet<&String> = answer.keys.iter().collect();
        let mut inserts = Vec::new();
        let mut updates = Vec::new();
        let mut hidden_updates = Vec::new();
        for (i, k) in read_keys.iter().flatten().enumerate() {
            let is_insert = i < window.inserts.len();
            match (visible.contains(k), is_insert) {
                (true, true) => inserts.push(k.clone()),
                (true, false) => updates.push(k.clone()),
                (false, false) => hidden_updates.push(k.clone()),
                (false, true) => {}
            }
        }
        Outcome::Checked(Checked {
            // A bucket's groups are never bypass groups (see the module docs).
            bypass: answer.bypass && self.bucket.is_none(),
            inserts,
            updates,
            hidden_updates,
            deletes: deletes.to_vec(),
        })
    }

    /// The check, polled until its xids are visible (P7): the permit is held only during each
    /// attempt. `None`: no permit, an error, a failed probe or never visible.
    async fn poll(&self, id: &CheckIdentity, req: &CheckRequest) -> Option<CheckAnswer> {
        let wait_until = Instant::now() + self.settings.visibility_wait;
        loop {
            let permit = self.limiter.try_permit()?;
            let answer = self.checker.check(&self.target, id, req).await;
            drop(permit);
            let answer = answer.ok()?;
            if !answer.probe_ok {
                return None;
            }
            if answer.visible {
                return Some(answer);
            }
            let now = Instant::now();
            if now >= wait_until {
                return None;
            }
            tokio::time::sleep_until((now + self.settings.visibility_poll).min(wait_until)).await;
        }
    }

    /// R15: whether the viewer's group sees each key (presence), through the R12 cache.
    /// `Err` (T7's contract, [`crate::source::ChangeSource::visible`]): treat as not visible.
    #[allow(clippy::result_unit_err)]
    pub async fn visible(&self, viewer: u64, keys: &[String]) -> Result<Vec<bool>, ()> {
        self.ask_keys(viewer, keys, true).ok_or(())?.run().await
    }

    /// The group's visibility check of `keys`, to [`KeyCheck::run`] without the filter;
    /// `cached`: answers from the R12 cache count (presence), otherwise every key is checked
    /// now (a view frame: a grant, a setting or a role attribute may have changed without any
    /// window since the cache was filled). The answers are cached either way. `None` for an
    /// unknown viewer or a relation whose key can't be checked.
    pub fn ask_keys(&self, viewer: u64, keys: &[String], cached: bool) -> Option<KeyCheck> {
        let v = self.viewers.get(&viewer)?;
        let group = self.group(&v.group)?;
        if !keys_supported(&self.relation) {
            return None;
        }
        let now = Instant::now();
        let mut out: Vec<Option<bool>> = Vec::with_capacity(keys.len());
        let mut ask: Vec<(usize, Vec<String>)> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            match parse_key(k, &self.relation) {
                None => out.push(Some(false)),
                Some(texts) => match cached
                    .then(|| self.cache.get(now, &v.group, self.relation.oid, k))
                    .flatten()
                {
                    Some(hit) => out.push(Some(hit)),
                    None => {
                        out.push(None);
                        ask.push((i, texts));
                    }
                },
            }
        }
        Some(KeyCheck {
            checker: self.checker.clone(),
            limiter: self.limiter.clone(),
            cache: self.cache.clone(),
            target: self.target.clone(),
            identity: group.identity.clone(),
            group: v.group.clone(),
            rel_oid: self.relation.oid,
            keys: keys.to_vec(),
            known: out,
            ask,
            generation: self.cache.generation(),
            bucket: self.bucket,
        })
    }
}

/// A key string (S11) as the relation's key texts: a JSON array of one string per key
/// column, each allowed (P6), in the canonical form ([`key_string`] of the texts gives it back).
fn parse_key(key: &str, rel: &Relation) -> Option<Vec<String>> {
    let texts: Vec<String> = serde_json::from_str(key).ok()?;
    if texts.len() != rel.key.len()
        || !texts
            .iter()
            .zip(&rel.key)
            .all(|(t, c)| key_text_allowed(c, t))
        || key_string(&texts) != key
    {
        return None;
    }
    Some(texts)
}

fn changed() -> Change {
    Change {
        op: ChangeOp::Changed,
        keys: None,
    }
}
