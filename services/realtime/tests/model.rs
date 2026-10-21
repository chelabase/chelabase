//! RT1 T8: the §4 invariant as a property test, on a fake (the live run is
//! `realtime_live::the_invariant_holds_against_postgres`).
//!
//! **Invariant:** the keys a viewer receives are exactly the keys of the window's rows that the
//! viewer's own console read returns at the check (inserts and updates); keyed deletes reach an
//! RLS viewer only for keys on its screen or delivered to it before (R19), and an update that
//! hides a row on its screen is a keyed delete (W26); a viewer whose read fails (no schema
//! `USAGE`, no SELECT on the key columns) gets `changed`, never keys (W2).
//!
//! Postgres is modelled per role: schema `USAGE`, table or column grants, `BYPASSRLS`, table
//! ownership with and without `FORCE ROW LEVEL SECURITY`, and policies that depend on the
//! session's `TimeZone` and `search_path` with role- and database-level `SET`s. Generated change
//! sequences interleave data changes (each one a window) with privilege, attribute and setting
//! changes (which fire no event: the per-window probe must catch them, R2).

mod fakes;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fakes::{BRANCH, DB_OID, REL, VERSION_OID, col, ks, relation_with, resolved, target};
use proptest::prelude::*;
use realtime::check::{
    CheckAnswer, CheckError, CheckRequest, CheckTarget, Checker, ComputeFacts, Limiter, Resolved,
    VisibilityCache,
};
use realtime::filter::{FilterSettings, TableFilter};
use realtime::merge::RelWindow;
use realtime::partition::Parent;
use realtime::protocol::ChangeOp;
use realtime::source::{ChannelEvent, CheckIdentity};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    None,
    Table,
    /// `GRANT SELECT (id)`: the key column only.
    KeyColumn,
    /// `GRANT SELECT (tag)`: another column only.
    OtherColumn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    A,
    B,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    /// `USING (tag = n)`.
    Tag(u8),
    /// `USING (extract(hour FROM ts) < h)`: the hour in the session's TimeZone.
    HourBefore(u8),
    /// `USING (allowed(id))`, a plpgsql function reading an unqualified `allow_list` through
    /// the session's `search_path` at run time.
    AllowList,
}

#[derive(Debug, Clone)]
struct RoleModel {
    usage: bool,
    select: Grant,
    bypassrls: bool,
    owns_table: bool,
    tz: Option<i8>,
    path: Option<Path>,
}

#[derive(Debug, Clone)]
struct Row {
    tag: u8,
    hour_utc: u8,
}

#[derive(Debug, Clone)]
struct Pg {
    roles: Vec<RoleModel>,
    rls: bool,
    force: bool,
    policy: Policy,
    db_tz: i8,
    db_path: Path,
    allow_a: BTreeSet<u32>,
    allow_b: BTreeSet<u32>,
    rows: HashMap<u32, Row>,
}

impl Pg {
    fn rls_active(&self, r: usize) -> bool {
        let role = &self.roles[r];
        self.rls && !role.bypassrls && !(role.owns_table && !self.force)
    }
    /// What `SELECT id::text FROM t` as role `r` (`None` = the owner) returns.
    fn read(&self, r: Option<usize>) -> Result<HashSet<u32>, ()> {
        let Some(r) = r else {
            // The owner: `postgres`, BYPASSRLS.
            return Ok(self.rows.keys().copied().collect());
        };
        let role = &self.roles[r];
        if !role.usage || !matches!(role.select, Grant::Table | Grant::KeyColumn) {
            return Err(());
        }
        if !self.rls_active(r) {
            return Ok(self.rows.keys().copied().collect());
        }
        let tz = role.tz.unwrap_or(self.db_tz);
        let path = role.path.unwrap_or(self.db_path);
        Ok(self
            .rows
            .iter()
            .filter(|(id, row)| match self.policy {
                Policy::Tag(t) => row.tag == t,
                Policy::HourBefore(h) => {
                    ((row.hour_utc as i16 + tz as i16).rem_euclid(24) as u8) < h
                }
                Policy::AllowList => match path {
                    Path::A => self.allow_a.contains(id),
                    Path::B => self.allow_b.contains(id),
                },
            })
            .map(|(id, _)| *id)
            .collect())
    }
}

/// RT1 final fix round: the key's type. The plugin prints a key one way (timestamps in UTC,
/// `+00`); each viewer's session prints it its own way (its `TimeZone`): the filter must match
/// the check's answer and the conversion in the session's form, never the plugin's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyKind {
    Int,
    /// `timestamptz` with microseconds: the session's text depends on its `TimeZone`.
    Timestamptz,
    /// `text` with quotes, a backslash and non-ASCII characters.
    Text,
}

fn key_kind() -> impl Strategy<Value = KeyKind> {
    prop_oneof![
        Just(KeyKind::Int),
        Just(KeyKind::Timestamptz),
        Just(KeyKind::Text)
    ]
}

/// `2026-01-<dd> hh:mm:ss.ffffff±hh` of row `id`'s key at the offset `tz` hours.
fn timestamp_text(id: u32, tz: i8) -> String {
    let minutes = i64::from(id) * 307 + i64::from(tz) * 60;
    let day = 10 + minutes.div_euclid(1440);
    let in_day = minutes.rem_euclid(1440);
    let sign = if tz < 0 { '-' } else { '+' };
    format!(
        "2026-01-{day:02} {:02}:{:02}:{:02}.{:06}{sign}{:02}",
        in_day / 60,
        in_day % 60,
        id % 60,
        u64::from(id) * 13 + 1,
        tz.unsigned_abs()
    )
}

/// How the plugin prints row `id`'s key.
fn plugin_text(kind: KeyKind, id: u32) -> String {
    match kind {
        KeyKind::Int => id.to_string(),
        KeyKind::Timestamptz => timestamp_text(id, 0),
        KeyKind::Text => format!("k{id} \"q\\\" é✓"),
    }
}

/// How a session at `tz` prints row `id`'s key.
fn session_text(kind: KeyKind, id: u32, tz: i8) -> String {
    match kind {
        KeyKind::Timestamptz => timestamp_text(id, tz),
        _ => plugin_text(kind, id),
    }
}

/// The row a key text names, in the plugin's form or any session's (Postgres reads both:
/// a check of a `view` frame binds the session's texts); `None`: none of the model's.
fn id_of(kind: KeyKind, text: &str) -> Option<u32> {
    (0..IDS + 3).find(|id| (-12i8..=14).any(|tz| session_text(kind, *id, tz) == text))
}

/// The key column's type for `kind`.
fn key_type(kind: KeyKind) -> &'static str {
    match kind {
        KeyKind::Int => "int4",
        KeyKind::Timestamptz => "timestamptz",
        KeyKind::Text => "text",
    }
}

fn table(kind: KeyKind) -> realtime::check::Resolved {
    resolved(relation_with(REL, "t", vec![col("id", key_type(kind))]))
}

impl Pg {
    /// The `TimeZone` of role `r`'s console session (`None` = the owner's).
    fn tz_of(&self, r: Option<usize>) -> i8 {
        r.and_then(|r| self.roles[r].tz).unwrap_or(self.db_tz)
    }
}

fn role_index(id: &CheckIdentity) -> Option<usize> {
    id.role
        .as_deref()
        .map(|r| r.trim_start_matches('r').parse().unwrap())
}

/// Answers every check from the model, as Postgres would in the viewer's console session.
struct ModelChecker {
    pg: Arc<Mutex<Pg>>,
    kind: KeyKind,
}

impl ModelChecker {
    /// Each plugin text in the session form of `r`'s session.
    fn converted(&self, pg: &Pg, r: Option<usize>, keys: &[Vec<String>]) -> Vec<Option<String>> {
        let tz = pg.tz_of(r);
        keys.iter()
            .map(|t| id_of(self.kind, &t[0]).map(|id| ks(&session_text(self.kind, id, tz))))
            .collect()
    }
}

#[async_trait]
impl Checker for ModelChecker {
    async fn compute(&self, _t: &CheckTarget) -> Result<ComputeFacts, CheckError> {
        Ok(ComputeFacts {
            read_only: false,
            max_replication_slots: 20,
            rt_slots: 0,
            db_oid: DB_OID,
            version_oid: Some(VERSION_OID),
        })
    }
    async fn resolve(
        &self,
        _t: &CheckTarget,
        _id: &CheckIdentity,
        _s: &str,
        _t2: &str,
    ) -> Result<Option<Resolved>, CheckError> {
        Ok(Some(table(self.kind)))
    }
    async fn parents(&self, _t: &CheckTarget, _rel: u32) -> Result<Vec<Parent>, CheckError> {
        Ok(vec![])
    }
    async fn check(
        &self,
        _t: &CheckTarget,
        id: &CheckIdentity,
        req: &CheckRequest,
    ) -> Result<CheckAnswer, CheckError> {
        let pg = self.pg.lock().unwrap();
        let r = role_index(id);
        let read = pg.read(r);
        let bypass = r.is_none_or(|r| !pg.rls_active(r));
        let tz = pg.tz_of(r);
        // The session's texts of the rows it reads (its own form), whatever form came in.
        let keys = match (&read, &req.keys) {
            (Ok(rows), Some(keys)) => keys
                .iter()
                .filter_map(|t| id_of(self.kind, &t[0]))
                .filter(|id| rows.contains(id))
                .map(|id| ks(&session_text(self.kind, id, tz)))
                .collect(),
            _ => vec![],
        };
        let converted = if read.is_ok() {
            self.converted(&pg, r, &req.convert)
        } else {
            vec![]
        };
        Ok(CheckAnswer {
            probe_ok: read.is_ok(),
            bypass,
            visible: true,
            keys,
            converted,
        })
    }
    async fn convert(
        &self,
        _t: &CheckTarget,
        id: &CheckIdentity,
        _rel: u32,
        keys: &[Vec<String>],
    ) -> Result<Vec<Option<String>>, CheckError> {
        let pg = self.pg.lock().unwrap();
        Ok(self.converted(&pg, role_index(id), keys))
    }
}

#[derive(Debug, Clone)]
enum Step {
    Insert { id: u32, tag: u8, hour: u8 },
    Update { id: u32, tag: u8, hour: u8 },
    Delete { id: u32 },
    Usage { role: usize, on: bool },
    Select { role: usize, grant: Grant },
    Bypass { role: usize, on: bool },
    Owns { role: usize, on: bool },
    Rls(bool),
    Force(bool),
    RoleTz { role: usize, tz: Option<i8> },
    DbTz(i8),
    RolePath { role: usize, path: Option<Path> },
    DbPath(Path),
    Allow { list: Path, id: u32, on: bool },
}

const ROLES: usize = 3;
const IDS: u32 = 6;

fn grant() -> impl Strategy<Value = Grant> {
    prop_oneof![
        Just(Grant::None),
        Just(Grant::Table),
        Just(Grant::KeyColumn),
        Just(Grant::OtherColumn)
    ]
}

fn path() -> impl Strategy<Value = Path> {
    prop_oneof![Just(Path::A), Just(Path::B)]
}

fn step() -> impl Strategy<Value = Step> {
    let id = 0..IDS;
    let role = 0..ROLES;
    prop_oneof![
        4 => (id.clone(), 0u8..3, 0u8..24).prop_map(|(id, tag, hour)| Step::Insert { id, tag, hour }),
        4 => (id.clone(), 0u8..3, 0u8..24).prop_map(|(id, tag, hour)| Step::Update { id, tag, hour }),
        3 => id.clone().prop_map(|id| Step::Delete { id }),
        1 => (role.clone(), any::<bool>()).prop_map(|(role, on)| Step::Usage { role, on }),
        1 => (role.clone(), grant()).prop_map(|(role, grant)| Step::Select { role, grant }),
        1 => (role.clone(), any::<bool>()).prop_map(|(role, on)| Step::Bypass { role, on }),
        1 => (role.clone(), any::<bool>()).prop_map(|(role, on)| Step::Owns { role, on }),
        1 => any::<bool>().prop_map(Step::Rls),
        1 => any::<bool>().prop_map(Step::Force),
        1 => (role.clone(), proptest::option::of(-12i8..=14)).prop_map(|(role, tz)| Step::RoleTz { role, tz }),
        1 => (-12i8..=14).prop_map(Step::DbTz),
        1 => (role.clone(), proptest::option::of(path())).prop_map(|(role, path)| Step::RolePath { role, path }),
        1 => path().prop_map(Step::DbPath),
        2 => (path(), id, any::<bool>()).prop_map(|(list, id, on)| Step::Allow { list, id, on }),
    ]
}

fn pg() -> impl Strategy<Value = Pg> {
    // Weighted toward RLS viewers that can read (where R19 and W26 have something to decide).
    let role = (
        proptest::bool::weighted(0.85),
        grant(),
        proptest::bool::weighted(0.15),
        proptest::bool::weighted(0.1),
        proptest::option::of(-12i8..=14),
        proptest::option::of(path()),
    )
        .prop_map(
            |(usage, select, bypassrls, owns_table, tz, path)| RoleModel {
                usage,
                select,
                bypassrls,
                owns_table,
                tz,
                path,
            },
        );
    let policy = prop_oneof![
        (0u8..3).prop_map(Policy::Tag),
        (1u8..24).prop_map(Policy::HourBefore),
        Just(Policy::AllowList)
    ];
    (
        proptest::collection::vec(role, ROLES),
        proptest::bool::weighted(0.85),
        any::<bool>(),
        policy,
        -12i8..=14,
        path(),
        proptest::collection::btree_set(0..IDS, 0..4),
        proptest::collection::btree_set(0..IDS, 0..4),
        // Rows that exist before anyone joins: visible to a viewer without ever being
        // delivered to it, so only a confirmed `view` makes their deletes keyed.
        proptest::collection::btree_map(0..IDS, (0u8..3, 0u8..24), 2..6),
    )
        .prop_map(
            |(roles, rls, force, policy, db_tz, db_path, allow_a, allow_b, rows)| Pg {
                roles,
                rls,
                force,
                policy,
                db_tz,
                db_path,
                allow_a,
                allow_b,
                rows: rows
                    .into_iter()
                    .map(|(id, (tag, hour_utc))| (id, Row { tag, hour_utc }))
                    .collect(),
            },
        )
}

/// A `view` frame before a step (or none): which of the viewer's visible keys it reports, which
/// of the keys its read would not return (hidden rows: guessed), and a key of no row at all.
#[derive(Debug, Clone)]
struct ViewPick {
    visible: u8,
    hidden: u8,
    ghost: Option<u32>,
}

fn view_pick() -> impl Strategy<Value = Option<ViewPick>> {
    proptest::option::weighted(
        0.8,
        (any::<u8>(), any::<u8>(), proptest::option::of(IDS..IDS + 3)).prop_map(
            |(visible, hidden, ghost)| ViewPick {
                visible,
                hidden,
                ghost,
            },
        ),
    )
}

/// The keys `pick` reports for a viewer whose read now returns `visible` (`None`: it fails),
/// among `existing` rows.
fn view_keys(
    pick: &ViewPick,
    visible: Option<&HashSet<u32>>,
    existing: &[u32],
    key: impl Fn(u32) -> String,
) -> Vec<String> {
    let mut out = vec![];
    let mut seen_visible = 0;
    let mut seen_hidden = 0;
    for id in existing {
        if visible.is_some_and(|v| v.contains(id)) {
            if pick.visible & (1 << (seen_visible % 8)) != 0 {
                out.push(key(*id));
            }
            seen_visible += 1;
        } else {
            if pick.hidden & (1 << (seen_hidden % 8)) != 0 {
                out.push(key(*id));
            }
            seen_hidden += 1;
        }
    }
    if let Some(g) = pick.ghost {
        out.push(key(g));
    }
    out
}

fn apply(pg: &mut Pg, s: &Step) {
    match *s {
        Step::Insert { id, tag, hour } | Step::Update { id, tag, hour } => {
            pg.rows.insert(
                id,
                Row {
                    tag,
                    hour_utc: hour,
                },
            );
        }
        Step::Delete { id } => {
            pg.rows.remove(&id);
        }
        Step::Usage { role, on } => pg.roles[role].usage = on,
        Step::Select { role, grant } => pg.roles[role].select = grant,
        Step::Bypass { role, on } => pg.roles[role].bypassrls = on,
        Step::Owns { role, on } => pg.roles[role].owns_table = on,
        Step::Rls(on) => pg.rls = on,
        Step::Force(on) => pg.force = on,
        Step::RoleTz { role, tz } => pg.roles[role].tz = tz,
        Step::DbTz(tz) => pg.db_tz = tz,
        Step::RolePath { role, path } => pg.roles[role].path = path,
        Step::DbPath(p) => pg.db_path = p,
        Step::Allow { list, id, on } => {
            let set = match list {
                Path::A => &mut pg.allow_a,
                Path::B => &mut pg.allow_b,
            };
            if on {
                set.insert(id);
            } else {
                set.remove(&id);
            }
        }
    }
}

/// The viewers: one per role, and the owner.
fn viewers() -> Vec<(u64, Option<usize>)> {
    let mut v: Vec<(u64, Option<usize>)> = (0..ROLES).map(|r| (r as u64, Some(r))).collect();
    v.push((99, None));
    v
}

fn identity(r: Option<usize>) -> CheckIdentity {
    CheckIdentity {
        role: r.map(|r| format!("r{r}")),
        token: Arc::from("member-token"),
    }
}

#[derive(Default, Debug)]
struct Received {
    inserts: BTreeSet<String>,
    updates: BTreeSet<String>,
    deletes: BTreeSet<String>,
    keyless_deletes: usize,
    changed: usize,
    resync: usize,
}

fn received(events: Vec<ChannelEvent>) -> Received {
    let mut r = Received::default();
    for ev in events {
        match ev {
            ChannelEvent::Rows(changes) => {
                for c in changes {
                    let keys = c.keys.clone().unwrap_or_default();
                    match (c.op, c.keys.is_some()) {
                        (ChangeOp::Insert, true) => r.inserts.extend(keys),
                        (ChangeOp::Update, true) => r.updates.extend(keys),
                        (ChangeOp::Delete, true) => r.deletes.extend(keys),
                        (ChangeOp::Delete, false) => r.keyless_deletes += 1,
                        (ChangeOp::Changed, _) => r.changed += 1,
                        other => panic!("{other:?}"),
                    }
                }
            }
            ChannelEvent::Resync { .. } => r.resync += 1,
            ChannelEvent::Status { .. } => {}
        }
    }
    r
}

/// The key strings of `ids` as a session at `tz` prints them.
fn keys_at(kind: KeyKind, tz: i8, ids: impl IntoIterator<Item = u32>) -> BTreeSet<String> {
    ids.into_iter()
        .map(|id| ks(&session_text(kind, id, tz)))
        .collect()
}

async fn run(
    initial: Pg,
    kind: KeyKind,
    steps: Vec<(Step, Option<ViewPick>)>,
) -> Result<(), TestCaseError> {
    let pg = Arc::new(Mutex::new(initial));
    let checker = Arc::new(ModelChecker {
        pg: pg.clone(),
        kind,
    });
    let mut filter = TableFilter::new(
        target(),
        table(kind).relation,
        checker,
        Limiter::new(8),
        VisibilityCache::new(Duration::from_millis(100)),
        FilterSettings::default(),
    );
    let mut rxs: HashMap<u64, mpsc::Receiver<ChannelEvent>> = HashMap::new();
    let mut delivered: HashMap<u64, BTreeSet<String>> = HashMap::new();
    // Keys confirmed visible through `view` (the last frame's).
    let mut confirmed: HashMap<u64, BTreeSet<String>> = HashMap::new();
    for (v, r) in viewers() {
        let snapshot = pg.lock().unwrap().clone();
        if snapshot.read(r).is_err() {
            continue; // the join's probe refuses it (forbidden)
        }
        let bypass = r.is_none_or(|r| !snapshot.rls_active(r));
        let (tx, rx) = mpsc::channel(1024);
        filter.add(v, identity(r), bypass, tx);
        rxs.insert(v, rx);
        delivered.insert(v, BTreeSet::new());
        confirmed.insert(v, BTreeSet::new());
    }
    let mut xid = 100u64;
    for (step, pick) in steps {
        // The client's `view` frame (when it sends one): its own visible keys mixed with
        // guessed hidden keys and keys of no row. Only those its read returns now count
        // (R19 as ruled), and they stay until the next frame replaces them.
        let before = pg.lock().unwrap().clone();
        if let Some(pick) = &pick {
            let mut existing: Vec<u32> = before.rows.keys().copied().collect();
            existing.sort();
            for (v, r) in viewers() {
                if rxs.contains_key(&v) {
                    let read = before.read(r).ok();
                    // The client's screen holds its own session's key strings.
                    let tz = before.tz_of(r);
                    let sent = view_keys(pick, read.as_ref(), &existing, |id| {
                        ks(&session_text(kind, id, tz))
                    });
                    let visible_now = read
                        .map(|rows| keys_at(kind, tz, rows))
                        .unwrap_or_default();
                    filter.view(v, sent.clone()).await;
                    confirmed.insert(
                        v,
                        sent.into_iter()
                            .filter(|k| visible_now.contains(k))
                            .collect(),
                    );
                }
            }
        }
        // Postgres decodes nothing for an INSERT that conflicts or an UPDATE/DELETE that
        // matches no row.
        let exists = |id: u32| before.rows.contains_key(&id);
        match step {
            Step::Insert { id, .. } if exists(id) => continue,
            Step::Update { id, .. } | Step::Delete { id } if !exists(id) => continue,
            _ => {}
        }
        apply(&mut pg.lock().unwrap(), &step);
        let (inserts, updates, deletes) = match step {
            Step::Insert { id, .. } => (vec![id], vec![], vec![]),
            Step::Update { id, .. } => (vec![], vec![id], vec![]),
            Step::Delete { id } => (vec![], vec![], vec![id]),
            // Privileges, attributes and settings fire no event: the next window's probe sees them.
            _ => continue,
        };
        xid += 1;
        let w = RelWindow {
            rel: REL,
            changed: false,
            // The plugin's texts (timestamps in UTC), never a session's.
            inserts: inserts.iter().map(|i| vec![plugin_text(kind, *i)]).collect(),
            updates: updates.iter().map(|i| vec![plugin_text(kind, *i)]).collect(),
            deletes: deletes.iter().map(|i| vec![plugin_text(kind, *i)]).collect(),
        };
        filter
            .deliver(&w, &[xid], tokio::time::Instant::now())
            .await;
        let after = pg.lock().unwrap().clone();
        for (v, r) in viewers() {
            let Some(rx) = rxs.get_mut(&v) else { continue };
            let got = received(fakes::drain(rx));
            let read = after.read(r);
            let ctx = format!("viewer {v} ({r:?}) after {step:?}: {got:?}; model {after:?}");
            prop_assert_eq!(got.resync, 0, "{}", ctx);
            let Ok(rows) = read else {
                // W2: no keys at all, only `changed`.
                prop_assert!(
                    got.inserts.is_empty() && got.updates.is_empty() && got.deletes.is_empty(),
                    "{}",
                    ctx
                );
                prop_assert_eq!(got.changed, 1, "{}", ctx);
                continue;
            };
            prop_assert_eq!(got.changed, 0, "nothing uncertain here: {}", ctx);
            let rls = r.is_some_and(|r| after.rls_active(r));
            // The viewer's session form at the check (after the step).
            let tz = after.tz_of(r);
            let visible = keys_at(kind, tz,rows.iter().copied());
            // Inserts and updates: exactly the window's keys the read returns.
            prop_assert_eq!(
                &got.inserts,
                &keys_at(kind, tz,inserts.iter().copied())
                    .intersection(&visible)
                    .cloned()
                    .collect(),
                "{}",
                ctx
            );
            prop_assert_eq!(
                &got.updates,
                &keys_at(kind, tz,updates.iter().copied())
                    .intersection(&visible)
                    .cloned()
                    .collect(),
                "{}",
                ctx
            );
            // A keyed delete only for a key that was visible to the viewer's own read when it
            // was confirmed: delivered as visible, or confirmed through `view` (never a guess).
            let known: BTreeSet<String> = confirmed[&v].union(&delivered[&v]).cloned().collect();
            if rls {
                let gone = keys_at(kind, tz,deletes.iter().copied());
                prop_assert_eq!(
                    &got.deletes
                        .intersection(&gone)
                        .cloned()
                        .collect::<BTreeSet<_>>(),
                    &gone.intersection(&known).cloned().collect::<BTreeSet<_>>(),
                    "R19 deletes: {}",
                    ctx
                );
                // W26: an update that hides a key: keyed for a key confirmed through `view`,
                // at most for the keys the viewer knows.
                let hidden: BTreeSet<String> = keys_at(kind, tz,updates.iter().copied())
                    .difference(&visible)
                    .cloned()
                    .collect();
                let hidden_got: BTreeSet<String> =
                    got.deletes.intersection(&hidden).cloned().collect();
                let on_screen: BTreeSet<String> =
                    hidden.intersection(&confirmed[&v]).cloned().collect();
                prop_assert!(
                    on_screen.is_subset(&hidden_got) && hidden_got.is_subset(&known),
                    "W26: {}",
                    ctx
                );
                prop_assert!(
                    got.deletes
                        .is_subset(&gone.union(&hidden).cloned().collect()),
                    "no other deletes: {}",
                    ctx
                );
            } else {
                prop_assert_eq!(&got.deletes, &keys_at(kind, tz,deletes.iter().copied()), "{}", ctx);
            }
            prop_assert_eq!(got.keyless_deletes, 0, "{}", ctx);
            let d = delivered.get_mut(&v).unwrap();
            d.extend(got.inserts.iter().cloned());
            d.extend(got.updates.iter().cloned());
            for k in &got.deletes {
                d.remove(k);
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn keys_a_viewer_receives_are_the_rows_its_own_read_returns(
        initial in pg(),
        kind in key_kind(),
        steps in proptest::collection::vec((step(), view_pick()), 1..30),
    ) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .start_paused(true)
            .build()
            .unwrap();
        rt.block_on(run(initial, kind, steps))?;
    }
}

#[test]
fn the_model_itself() {
    // A sanity check of the oracle: RLS by TimeZone and by search_path.
    let mut pg = Pg {
        roles: vec![RoleModel {
            usage: true,
            select: Grant::KeyColumn,
            bypassrls: false,
            owns_table: false,
            tz: Some(9),
            path: Some(Path::B),
        }],
        rls: true,
        force: false,
        policy: Policy::HourBefore(12),
        db_tz: 0,
        db_path: Path::A,
        allow_a: BTreeSet::from([1]),
        allow_b: BTreeSet::from([2]),
        rows: HashMap::from([
            (
                1,
                Row {
                    tag: 0,
                    hour_utc: 1,
                },
            ),
            (
                2,
                Row {
                    tag: 0,
                    hour_utc: 5,
                },
            ),
        ]),
    };
    assert_eq!(
        pg.read(Some(0)),
        Ok(HashSet::from([1])),
        "05:00 UTC is 14:00 in +09"
    );
    pg.policy = Policy::AllowList;
    assert_eq!(
        pg.read(Some(0)),
        Ok(HashSet::from([2])),
        "the role's search_path"
    );
    pg.roles[0].usage = false;
    assert_eq!(pg.read(Some(0)), Err(()));
    let _ = BRANCH;
    // The key forms: one instant, printed per session; every form names its row.
    assert_eq!(timestamp_text(1, 0), "2026-01-10 05:07:01.000014+00");
    assert_eq!(timestamp_text(1, 9), "2026-01-10 14:07:01.000014+09");
    assert_eq!(timestamp_text(0, -12), "2026-01-09 12:00:00.000001-12");
    for kind in [KeyKind::Int, KeyKind::Timestamptz, KeyKind::Text] {
        for id in 0..IDS + 3 {
            assert_eq!(id_of(kind, &plugin_text(kind, id)), Some(id), "{kind:?}");
            assert_eq!(id_of(kind, &session_text(kind, id, -5)), Some(id), "{kind:?}");
        }
    }
    assert_ne!(
        session_text(KeyKind::Timestamptz, 3, 9),
        plugin_text(KeyKind::Timestamptz, 3),
        "a session's form isn't the plugin's"
    );
}
