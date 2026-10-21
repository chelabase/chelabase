//! T8's fakes: a [`Checker`] modelling Postgres per role (which rows exist, which a role's RLS
//! hides, bypass or not, the probe, visibility of xids, conversion to the session's key form) and
//! a [`Replicator`] whose walsender the test drives. Shared by `filter.rs`, `source.rs` and
//! `model.rs`.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use chelabase_query::realtime::{KeyColumn, Relation, key_string};
use realtime::check::{
    CheckAnswer, CheckError, CheckRequest, CheckTarget, Checker, ComputeFacts, GroupKey, Resolved,
};
use realtime::partition::Parent;
use realtime::slots::{Replicator, SlotError, SlotTarget, WalMessage, WalStream};
use realtime::source::{ChannelEvent, CheckIdentity};
use tokio::sync::{Notify, Semaphore, mpsc};
use tokio_postgres::types::PgLsn;
use uuid::Uuid;

pub const DB: Uuid = Uuid::from_u128(0x1111_1111_1111_4111_8111_1111_1111_1111);
pub const BRANCH: Uuid = Uuid::from_u128(0x2222_2222_2222_4222_8222_2222_2222_2222);
pub const DB_OID: u32 = 16_384;
pub const REL: u32 = 16_400;
pub const VERSION_OID: u32 = 16_390;

pub fn target() -> CheckTarget {
    CheckTarget {
        database_id: DB,
        branch_id: BRANCH,
        pg_database: "app".into(),
    }
}

pub fn role(name: &str) -> CheckIdentity {
    CheckIdentity {
        role: Some(name.into()),
        token: Arc::from(format!("token-of-{name}")),
    }
}

/// The owner, with a member's own token.
pub fn owner(token: &str) -> CheckIdentity {
    CheckIdentity {
        role: None,
        token: Arc::from(token),
    }
}

pub fn col(name: &str, ty: &str) -> KeyColumn {
    KeyColumn {
        name: name.into(),
        type_schema: "pg_catalog".into(),
        type_name: ty.into(),
        base_schema: "pg_catalog".into(),
        base_name: ty.into(),
        base_typtype: 'b',
    }
}

/// `public.<name>` with an `int4` key `id`.
pub fn relation(oid: u32, name: &str) -> Relation {
    relation_with(oid, name, vec![col("id", "int4")])
}

pub fn relation_with(oid: u32, name: &str, key: Vec<KeyColumn>) -> Relation {
    Relation {
        oid,
        schema: "public".into(),
        name: name.into(),
        key,
    }
}

pub fn resolved(rel: Relation) -> Resolved {
    Resolved {
        relation: rel,
        relkind: 'r',
        inheritance_parent: false,
    }
}

/// `["<text>"]`.
pub fn ks(text: &str) -> String {
    key_string(&[text.to_string()])
}

pub fn texts(text: &str) -> Vec<String> {
    vec![text.to_string()]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    Compute,
    Resolve {
        role: GroupKey,
        schema: String,
        table: String,
    },
    Parents(u32),
    Check {
        role: GroupKey,
        rel: u32,
        xids: Vec<u64>,
        keys: Option<Vec<Vec<String>>>,
    },
    Convert {
        role: GroupKey,
        rel: u32,
        keys: Vec<Vec<String>>,
    },
}

#[derive(Default)]
struct State {
    compute: Option<Result<ComputeFacts, CheckError>>,
    /// `pg_database` → its OID (default [`DB_OID`]).
    db_oids: HashMap<String, u32>,
    resolve: HashMap<(String, String), Result<Option<Resolved>, CheckError>>,
    parents: HashMap<u32, Vec<Parent>>,
    /// Key strings of the rows that exist, per relation.
    rows: HashMap<u32, HashSet<String>>,
    /// Key strings a role's RLS hides.
    hidden: HashMap<GroupKey, HashSet<String>>,
    bypass: HashMap<GroupKey, bool>,
    probe_fails: HashSet<GroupKey>,
    fail: HashMap<GroupKey, CheckError>,
    /// Polls left before an xid is visible.
    invisible: HashMap<u64, usize>,
    never_visible: HashSet<u64>,
    /// Plugin texts → the session's key string (default: `key_string` of the texts).
    convert: HashMap<Vec<String>, String>,
    calls: Vec<Call>,
    /// The identity token every check carried, with its group.
    tokens: Vec<(GroupKey, String)>,
    in_flight: usize,
    max_in_flight: usize,
    /// Calls of `Checker::convert` (a conversion in a transaction of its own).
    separate_converts: usize,
    /// Each check's `convert` keys (converted in the check's own session).
    check_converts: Vec<Vec<Vec<String>>>,
}

/// See the module docs. Every role is a bypass role unless [`FakeChecker::set_bypass`] says
/// otherwise; the owner (`None`) too.
pub struct FakeChecker {
    state: Mutex<State>,
    /// While set, every check waits for a permit of this semaphore (none until released).
    gate: Mutex<Option<Arc<Semaphore>>>,
    pub checked: Notify,
}

impl FakeChecker {
    pub fn new() -> Arc<Self> {
        let me = Arc::new(Self {
            state: Mutex::new(State::default()),
            gate: Mutex::new(None),
            checked: Notify::new(),
        });
        me.set_compute(Ok(ComputeFacts {
            read_only: false,
            max_replication_slots: 20,
            rt_slots: 0,
            db_oid: DB_OID,
            version_oid: Some(VERSION_OID),
        }));
        me.set_resolve(
            "public",
            "items",
            Ok(Some(resolved(relation(REL, "items")))),
        );
        me
    }
    pub fn set_compute(&self, facts: Result<ComputeFacts, CheckError>) {
        self.state.lock().unwrap().compute = Some(facts);
    }
    pub fn update_compute(&self, f: impl FnOnce(&mut ComputeFacts)) {
        let mut s = self.state.lock().unwrap();
        if let Some(Ok(facts)) = s.compute.as_mut() {
            f(facts);
        }
    }
    /// Another Postgres database of the same compute.
    pub fn set_db_oid(&self, pg_database: &str, oid: u32) {
        self.state
            .lock()
            .unwrap()
            .db_oids
            .insert(pg_database.into(), oid);
    }
    pub fn set_resolve(&self, schema: &str, table: &str, r: Result<Option<Resolved>, CheckError>) {
        self.state
            .lock()
            .unwrap()
            .resolve
            .insert((schema.into(), table.into()), r);
    }
    pub fn set_parents(&self, rel: u32, parents: Vec<Parent>) {
        self.state.lock().unwrap().parents.insert(rel, parents);
    }
    /// Rows (key strings) that exist in `rel`.
    pub fn insert_rows(&self, rel: u32, keys: &[&str]) {
        let mut s = self.state.lock().unwrap();
        let set = s.rows.entry(rel).or_default();
        for k in keys {
            set.insert(ks(k));
        }
    }
    pub fn delete_rows(&self, rel: u32, keys: &[&str]) {
        let mut s = self.state.lock().unwrap();
        if let Some(set) = s.rows.get_mut(&rel) {
            for k in keys {
                set.remove(&ks(k));
            }
        }
    }
    /// The role's RLS hides these keys (and makes the role an RLS role).
    pub fn hide(&self, role: Option<&str>, keys: &[&str]) {
        let mut s = self.state.lock().unwrap();
        let g = role.map(str::to_string);
        s.bypass.insert(g.clone(), false);
        let set = s.hidden.entry(g).or_default();
        for k in keys {
            set.insert(ks(k));
        }
    }
    pub fn unhide(&self, role: Option<&str>, keys: &[&str]) {
        let mut s = self.state.lock().unwrap();
        if let Some(set) = s.hidden.get_mut(&role.map(str::to_string)) {
            for k in keys {
                set.remove(&ks(k));
            }
        }
    }
    pub fn set_bypass(&self, role: Option<&str>, bypass: bool) {
        self.state
            .lock()
            .unwrap()
            .bypass
            .insert(role.map(str::to_string), bypass);
    }
    pub fn fail_probe(&self, role: Option<&str>, fail: bool) {
        let mut s = self.state.lock().unwrap();
        let g = role.map(str::to_string);
        if fail {
            s.probe_fails.insert(g);
        } else {
            s.probe_fails.remove(&g);
        }
    }
    pub fn fail_checks(&self, role: Option<&str>, err: Option<CheckError>) {
        let mut s = self.state.lock().unwrap();
        let g = role.map(str::to_string);
        match err {
            Some(e) => {
                s.fail.insert(g, e);
            }
            None => {
                s.fail.remove(&g);
            }
        }
    }
    /// The xid becomes visible after `polls` more checks that carry it.
    pub fn invisible_for(&self, xid: u64, polls: usize) {
        self.state.lock().unwrap().invisible.insert(xid, polls);
    }
    pub fn never_visible(&self, xid: u64) {
        self.state.lock().unwrap().never_visible.insert(xid);
    }
    pub fn convert_as(&self, plugin_texts: &[&str], key: &str) {
        self.state.lock().unwrap().convert.insert(
            plugin_texts.iter().map(|s| s.to_string()).collect(),
            key.to_string(),
        );
    }
    /// From now on every check waits until [`FakeChecker::release`].
    pub fn hold(&self) {
        *self.gate.lock().unwrap() = Some(Arc::new(Semaphore::new(0)));
    }
    pub fn release(&self) {
        if let Some(g) = self.gate.lock().unwrap().take() {
            g.add_permits(Semaphore::MAX_PERMITS / 2);
        }
    }
    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }
    pub fn checks(&self) -> Vec<Call> {
        self.calls()
            .into_iter()
            .filter(|c| matches!(c, Call::Check { .. }))
            .collect()
    }
    pub fn checks_of(&self, role: Option<&str>) -> usize {
        let g = role.map(str::to_string);
        self.checks()
            .iter()
            .filter(|c| matches!(c, Call::Check { role, .. } if *role == g))
            .count()
    }
    /// The tokens the checks of `role`'s group carried, in order (`None` = the owner's).
    pub fn check_tokens(&self, role: Option<&str>) -> Vec<String> {
        let g = role.map(str::to_string);
        self.state
            .lock()
            .unwrap()
            .tokens
            .iter()
            .filter(|(r, _)| *r == g)
            .map(|(_, t)| t.clone())
            .collect()
    }
    pub fn in_flight(&self) -> usize {
        self.state.lock().unwrap().in_flight
    }
    pub fn max_in_flight(&self) -> usize {
        self.state.lock().unwrap().max_in_flight
    }
    /// RT1 final fix round: conversions made apart from a check (their own transaction).
    pub fn separate_converts(&self) -> usize {
        self.state.lock().unwrap().separate_converts
    }
    /// The `convert` keys every check carried, in order.
    pub fn check_converts(&self) -> Vec<Vec<Vec<String>>> {
        self.state.lock().unwrap().check_converts.clone()
    }
    pub fn clear_calls(&self) {
        self.state.lock().unwrap().calls.clear();
    }

    fn key_of(s: &State, texts: &[String]) -> String {
        s.convert
            .get(texts)
            .cloned()
            .unwrap_or_else(|| key_string(texts))
    }
}

#[async_trait]
impl Checker for FakeChecker {
    async fn compute(&self, target: &CheckTarget) -> Result<ComputeFacts, CheckError> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(Call::Compute);
        let oid = s.db_oids.get(&target.pg_database).copied();
        s.compute.clone().unwrap().map(|mut f| {
            if let Some(oid) = oid {
                f.db_oid = oid;
            }
            f
        })
    }
    async fn resolve(
        &self,
        _target: &CheckTarget,
        id: &CheckIdentity,
        schema: &str,
        table: &str,
    ) -> Result<Option<Resolved>, CheckError> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(Call::Resolve {
            role: id.role.clone(),
            schema: schema.into(),
            table: table.into(),
        });
        s.resolve
            .get(&(schema.to_string(), table.to_string()))
            .cloned()
            .unwrap_or(Ok(None))
    }
    async fn parents(
        &self,
        _target: &CheckTarget,
        rel_oid: u32,
    ) -> Result<Vec<Parent>, CheckError> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(Call::Parents(rel_oid));
        Ok(s.parents.get(&rel_oid).cloned().unwrap_or_default())
    }
    async fn check(
        &self,
        _target: &CheckTarget,
        id: &CheckIdentity,
        req: &CheckRequest,
    ) -> Result<CheckAnswer, CheckError> {
        let g = id.role.clone();
        {
            let mut s = self.state.lock().unwrap();
            s.tokens.push((g.clone(), id.token.to_string()));
            s.calls.push(Call::Check {
                role: g.clone(),
                rel: req.rel_oid,
                xids: req.xids.clone(),
                keys: req.keys.clone(),
            });
            s.check_converts.push(req.convert.clone());
            if !req.convert.is_empty() {
                // Converted in the check's session: recorded like a conversion, for the tests
                // that look at which keys were converted as whom.
                s.calls.push(Call::Convert {
                    role: g.clone(),
                    rel: req.rel_oid,
                    keys: req.convert.clone(),
                });
            }
            s.in_flight += 1;
            s.max_in_flight = s.max_in_flight.max(s.in_flight);
        }
        self.checked.notify_waiters();
        let gate = self.gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            let _ = gate.acquire().await;
        }
        let mut s = self.state.lock().unwrap();
        s.in_flight -= 1;
        if let Some(e) = s.fail.get(&g) {
            return Err(e.clone());
        }
        let probe_ok = !s.probe_fails.contains(&g);
        let bypass = s.bypass.get(&g).copied().unwrap_or(true);
        let mut visible = true;
        for x in &req.xids {
            if s.never_visible.contains(x) {
                visible = false;
            }
            if let Some(left) = s.invisible.get_mut(x)
                && *left > 0
            {
                *left -= 1;
                visible = false;
            }
        }
        let mut keys = vec![];
        if probe_ok
            && visible
            && let Some(req_keys) = &req.keys
        {
            let rows = s.rows.get(&req.rel_oid).cloned().unwrap_or_default();
            let hidden = s.hidden.get(&g).cloned().unwrap_or_default();
            for t in req_keys {
                let k = Self::key_of(&s, t);
                if rows.contains(&k) && (bypass || !hidden.contains(&k)) && !keys.contains(&k) {
                    keys.push(k);
                }
            }
        }
        // The session's form of every `convert` key (rows needn't exist); none for a failed
        // probe (nothing the session printed can be trusted).
        let converted = if probe_ok {
            req.convert
                .iter()
                .map(|t| Some(Self::key_of(&s, t)))
                .collect()
        } else {
            Vec::new()
        };
        Ok(CheckAnswer {
            probe_ok,
            bypass,
            visible,
            keys,
            converted,
        })
    }
    async fn convert(
        &self,
        _target: &CheckTarget,
        id: &CheckIdentity,
        rel_oid: u32,
        keys: &[Vec<String>],
    ) -> Result<Vec<Option<String>>, CheckError> {
        let mut s = self.state.lock().unwrap();
        s.separate_converts += 1;
        s.calls.push(Call::Convert {
            role: id.role.clone(),
            rel: rel_oid,
            keys: keys.to_vec(),
        });
        if let Some(e) = s.fail.get(&id.role) {
            return Err(e.clone());
        }
        Ok(keys.iter().map(|t| Some(Self::key_of(&s, t))).collect())
    }
}

/// Drains every event a sink holds now.
pub fn drain(rx: &mut mpsc::Receiver<ChannelEvent>) -> Vec<ChannelEvent> {
    let mut out = vec![];
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

// --- the replicator ---

/// One fake walsender session: the test pushes messages; the source's confirmations are
/// recorded; dropping the stream (the source closing its connection) is observable.
pub struct FakeSession {
    pub slot: String,
    pub target: SlotTarget,
    tx: mpsc::UnboundedSender<Option<Result<WalMessage, SlotError>>>,
    confirms: Arc<Mutex<Vec<PgLsn>>>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl FakeSession {
    /// A committed transaction's plugin message, at `wal_end`.
    pub fn data(&self, wal_end: u64, json: &str) {
        let _ = self.tx.send(Some(Ok(WalMessage::Data {
            wal_end: PgLsn::from(wal_end),
            data: Bytes::from(json.to_string()),
        })));
    }
    pub fn keepalive(&self, wal_end: u64) {
        let _ = self.tx.send(Some(Ok(WalMessage::Keepalive {
            wal_end: PgLsn::from(wal_end),
            reply: false,
        })));
    }
    /// The walsender goes away (a compute restart): the stream ends.
    pub fn end(&self) {
        let _ = self.tx.send(None);
    }
    pub fn confirms(&self) -> Vec<PgLsn> {
        self.confirms.lock().unwrap().clone()
    }
    pub fn last_confirm(&self) -> Option<PgLsn> {
        self.confirms.lock().unwrap().last().copied()
    }
    /// Whether the source dropped its stream (closed the connection).
    pub fn closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }
}

struct FakeStream {
    rx: mpsc::UnboundedReceiver<Option<Result<WalMessage, SlotError>>>,
    confirms: Arc<Mutex<Vec<PgLsn>>>,
    closed: Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for FakeStream {
    fn drop(&mut self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl WalStream for FakeStream {
    async fn next(&mut self) -> Option<Result<WalMessage, SlotError>> {
        self.rx.recv().await.flatten()
    }
    async fn confirm(&mut self, flush: PgLsn) -> Result<(), SlotError> {
        self.confirms.lock().unwrap().push(flush);
        Ok(())
    }
}

/// What the next `start` does.
#[derive(Debug, Clone)]
pub enum StartPlan {
    /// Streams (a [`FakeSession`] appears in `sessions`).
    Stream,
    /// Never returns (creation blocked by a running transaction).
    Block,
    Fail(SlotError),
}

#[derive(Default)]
struct ReplState {
    plans: VecDeque<StartPlan>,
    starts: Vec<(SlotTarget, String)>,
    sessions: Vec<Arc<FakeSession>>,
    /// Blocked starts whose future was dropped.
    abandoned: usize,
}

/// The default plan is [`StartPlan::Stream`].
#[derive(Default)]
pub struct FakeReplicator {
    state: Mutex<ReplState>,
    pub started: Notify,
}

struct Abandon<'a>(&'a FakeReplicator, bool);
impl Drop for Abandon<'_> {
    fn drop(&mut self) {
        if !self.1 {
            self.0.state.lock().unwrap().abandoned += 1;
        }
    }
}

impl FakeReplicator {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    pub fn plan(&self, p: StartPlan) {
        self.state.lock().unwrap().plans.push_back(p);
    }
    pub fn starts(&self) -> usize {
        self.state.lock().unwrap().starts.len()
    }
    pub fn start_targets(&self) -> Vec<SlotTarget> {
        self.state
            .lock()
            .unwrap()
            .starts
            .iter()
            .map(|(t, _)| t.clone())
            .collect()
    }
    pub fn sessions(&self) -> Vec<Arc<FakeSession>> {
        self.state.lock().unwrap().sessions.clone()
    }
    pub fn session(&self) -> Option<Arc<FakeSession>> {
        self.state.lock().unwrap().sessions.last().cloned()
    }
    /// Blocked creations the source gave up on (closed the connection).
    pub fn abandoned(&self) -> usize {
        self.state.lock().unwrap().abandoned
    }
}

#[async_trait]
impl Replicator for FakeReplicator {
    async fn start(
        &self,
        target: &SlotTarget,
        slot: &str,
    ) -> Result<Box<dyn WalStream>, SlotError> {
        let plan = {
            let mut s = self.state.lock().unwrap();
            s.starts.push((target.clone(), slot.to_string()));
            s.plans.pop_front().unwrap_or(StartPlan::Stream)
        };
        self.started.notify_waiters();
        match plan {
            StartPlan::Stream => {
                let (tx, rx) = mpsc::unbounded_channel();
                let confirms = Arc::new(Mutex::new(vec![]));
                let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let session = Arc::new(FakeSession {
                    slot: slot.to_string(),
                    target: target.clone(),
                    tx,
                    confirms: confirms.clone(),
                    closed: closed.clone(),
                });
                self.state.lock().unwrap().sessions.push(session);
                Ok(Box::new(FakeStream {
                    rx,
                    confirms,
                    closed,
                }))
            }
            StartPlan::Block => {
                let guard = Abandon(self, false);
                std::future::pending::<()>().await;
                drop(guard);
                unreachable!()
            }
            StartPlan::Fail(e) => Err(e),
        }
    }
}
