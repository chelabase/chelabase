//! The merge window (spec §3.3, §8.1): decoded transactions are merged per relation over
//! [`WINDOW`] from the first one; a relation with more than [`MAX_KEYS`] distinct keys in a
//! window, or a `c` change, is `changed`; an `all` transaction makes the whole window `all`.
//!
//! Each key appears once per relation per window, with one op:
//!
//! | ops of the key in the window, in decode order | op |
//! |---|---|
//! | only inserts (updates after an insert included) | insert |
//! | ends with a delete (in its own transaction) | delete |
//! | a delete followed by an insert | update |
//! | an insert and a delete in **one** transaction (order unknown) | update (re-read) |
//! | anything else | update |
//!
//! Keys keep their first-seen order. The window carries every transaction's xid (in decode
//! order) and the highest end LSN.

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;
use tokio_postgres::types::PgLsn;

use crate::decoder::{Body, Op, Txn};

pub const WINDOW: Duration = Duration::from_millis(100);
pub const MAX_KEYS: usize = 1000;

/// One relation's merged changes: key column texts (the plugin's form, not yet key strings).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelWindow {
    pub rel: u32,
    /// Keyless: refetch the page (`c`, over [`MAX_KEYS`], or a non-keyed fan-out target).
    pub changed: bool,
    pub inserts: Vec<Vec<String>>,
    pub updates: Vec<Vec<String>>,
    pub deletes: Vec<Vec<String>>,
}

impl RelWindow {
    /// Distinct keys over the three ops.
    pub fn key_count(&self) -> usize {
        self.inserts.len() + self.updates.len() + self.deletes.len()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Window {
    /// An `all` transaction (or an unreadable message) was in the window.
    pub all: bool,
    /// In first-change order.
    pub rels: Vec<RelWindow>,
    /// Every transaction's xid, in decode order.
    pub xids: Vec<u64>,
    /// The highest end LSN in the window (what is acknowledged once it's published).
    pub end_lsn: Option<PgLsn>,
}

impl Window {
    pub fn rel(&self, oid: u32) -> Option<&RelWindow> {
        self.rels.iter().find(|r| r.rel == oid)
    }
}

/// What a key's transactions so far amount to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyOp {
    Insert,
    Update,
    Delete,
}

/// One relation's keys in the open window: each key's op, keys in first-seen order.
#[derive(Default)]
struct RelState {
    changed: bool,
    order: Vec<Vec<String>>,
    ops: HashMap<Vec<String>, KeyOp>,
}

impl RelState {
    /// One transaction's ops of `key` (`i`, `u`, `d` seen in it) folded into its window op.
    fn apply(&mut self, key: &[String], ins: bool, del: bool, max_keys: usize) {
        if self.changed {
            return;
        }
        let txn_op = match (ins, del) {
            // The order inside one transaction is lost: the row may exist or not, re-read it.
            (true, true) => KeyOp::Update,
            (false, true) => KeyOp::Delete,
            (true, false) => KeyOp::Insert,
            (false, false) => KeyOp::Update,
        };
        let next = match (self.ops.get(key).copied(), txn_op) {
            (_, KeyOp::Delete) if !ins => KeyOp::Delete,
            (None, op) => op,
            // An insert followed only by updates stays an insert.
            (Some(KeyOp::Insert), KeyOp::Update) if !ins && !del => KeyOp::Insert,
            (Some(_), _) => KeyOp::Update,
        };
        if !self.ops.contains_key(key) {
            if self.order.len() >= max_keys {
                self.changed = true;
                self.order.clear();
                self.ops.clear();
                return;
            }
            self.order.push(key.to_vec());
        }
        self.ops.insert(key.to_vec(), next);
    }
    fn into_window(self, rel: u32) -> RelWindow {
        let mut w = RelWindow {
            rel,
            changed: self.changed,
            ..RelWindow::default()
        };
        if self.changed {
            return w;
        }
        for key in self.order {
            match self.ops[&key] {
                KeyOp::Insert => w.inserts.push(key),
                KeyOp::Update => w.updates.push(key),
                KeyOp::Delete => w.deletes.push(key),
            }
        }
        w
    }
}

/// One key's ops in one transaction: the texts, inserted, deleted.
type TxnKey = (Vec<String>, bool, bool);

/// Accumulates one window at a time.
pub struct Merger {
    max_keys: usize,
    opened: Option<Instant>,
    all: bool,
    order: Vec<u32>,
    rels: HashMap<u32, RelState>,
    xids: Vec<u64>,
    end_lsn: Option<PgLsn>,
}

impl Merger {
    pub fn new(max_keys: usize) -> Self {
        Self {
            max_keys,
            opened: None,
            all: false,
            order: Vec::new(),
            rels: HashMap::new(),
            xids: Vec::new(),
            end_lsn: None,
        }
    }
    /// Adds a decoded transaction; the first one of a window opens it at `at`.
    pub fn push(&mut self, at: Instant, txn: &Txn) {
        if self.opened.is_none() {
            self.opened = Some(at);
        }
        if let Some(xid) = txn.xid {
            self.xids.push(xid);
        }
        if let Some(lsn) = txn.lsn
            && self.end_lsn.is_none_or(|e| lsn > e)
        {
            self.end_lsn = Some(lsn);
        }
        let changes = match &txn.body {
            Body::All => {
                self.all = true;
                return;
            }
            Body::Changes(changes) => changes,
        };
        // Per relation, each key's ops in this transaction (the plugin lists i, u, d groups).
        let mut per_rel: Vec<(u32, Vec<TxnKey>)> = Vec::new();
        for c in changes {
            if !self.rels.contains_key(&c.rel) {
                self.order.push(c.rel);
                self.rels.insert(c.rel, RelState::default());
            }
            let idx = match per_rel.iter().position(|(r, _)| *r == c.rel) {
                Some(i) => i,
                None => {
                    per_rel.push((c.rel, Vec::new()));
                    per_rel.len() - 1
                }
            };
            if c.op == Op::Changed {
                let state = self.rels.get_mut(&c.rel).expect("inserted above");
                state.changed = true;
                state.order.clear();
                state.ops.clear();
                continue;
            }
            let keys = &mut per_rel[idx].1;
            for key in &c.keys {
                let entry = match keys.iter().position(|(k, _, _)| k == key) {
                    Some(i) => &mut keys[i],
                    None => {
                        keys.push((key.clone(), false, false));
                        keys.last_mut().expect("just pushed")
                    }
                };
                match c.op {
                    Op::Insert => entry.1 = true,
                    Op::Delete => entry.2 = true,
                    Op::Update | Op::Changed => {}
                }
            }
        }
        for (rel, keys) in per_rel {
            let state = self.rels.get_mut(&rel).expect("inserted above");
            for (key, ins, del) in keys {
                state.apply(&key, ins, del, self.max_keys);
            }
        }
    }
    pub fn is_empty(&self) -> bool {
        self.opened.is_none()
    }
    /// When the open window is due: its opening + [`WINDOW`].
    pub fn due_at(&self) -> Option<Instant> {
        self.opened.map(|o| o + WINDOW)
    }
    /// The open window (and a fresh one starts with the next push).
    pub fn take(&mut self) -> Option<Window> {
        self.opened.take()?;
        let mut rels = std::mem::take(&mut self.rels);
        let window = Window {
            all: std::mem::take(&mut self.all),
            rels: std::mem::take(&mut self.order)
                .into_iter()
                .map(|rel| {
                    rels.remove(&rel)
                        .expect("every listed relation has a state")
                        .into_window(rel)
                })
                .collect(),
            xids: std::mem::take(&mut self.xids),
            end_lsn: self.end_lsn.take(),
        };
        Some(window)
    }
}
