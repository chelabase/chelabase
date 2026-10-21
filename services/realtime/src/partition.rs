//! Relation kinds and partition trees (R16, W25).
//!
//! - A change to a partition reaches the channels of the partition itself and of every
//!   `pg_partition_ancestors` entry (intermediate and root), keyed: each channel is checked
//!   against its own joined relation, so that relation's RLS applies.
//! - A legacy inheritance parent reads its children's rows: a child's change reaches it as
//!   keyless `changed`.
//! - Views, materialized views and foreign tables join with `live: false, reason:
//!   "not_a_table"`.

use std::collections::HashMap;

use crate::merge::{RelWindow, Window};

/// A relation a change also counts for: a partition ancestor (`keyed`) or an inheritance
/// parent (not keyed: `changed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parent {
    pub oid: u32,
    pub keyed: bool,
}

/// For a relation OID `$1`: its partition ancestors other than itself (`keyed = true`) and its
/// inheritance parents (`keyed = false`).
pub const PARENTS_SQL: &str = "SELECT a.relid::pg_catalog.oid, true \
     FROM pg_catalog.pg_partition_ancestors($1::pg_catalog.regclass) AS a \
     WHERE a.relid OPERATOR(pg_catalog.<>) $1::pg_catalog.regclass \
     UNION ALL \
     SELECT i.inhparent, false FROM pg_catalog.pg_inherits i \
     JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.inhparent \
     WHERE i.inhrelid OPERATOR(pg_catalog.=) $1::pg_catalog.oid \
       AND c.relkind OPERATOR(pg_catalog.<>) 'p'";

/// The window's relations fanned out to their parents (`parents[rel]`, absent = none): each
/// relation keeps its own window and every parent gets the union of its descendants' keys
/// (keyed parents) or `changed` (inheritance parents). A target over `max_keys` distinct keys
/// after the union is `changed`.
pub fn fan_out(
    window: &Window,
    parents: &HashMap<u32, Vec<Parent>>,
    max_keys: usize,
) -> Vec<RelWindow> {
    if parents.is_empty() {
        return window.rels.clone();
    }
    // Targets in first-reached order: each relation's own window, then its parents.
    let mut order: Vec<u32> = Vec::new();
    let mut targets: HashMap<u32, Target> = HashMap::new();
    for r in &window.rels {
        reach(&mut targets, &mut order, r.rel).add(r, true);
        for p in parents.get(&r.rel).into_iter().flatten() {
            reach(&mut targets, &mut order, p.oid).add(r, p.keyed);
        }
    }
    order
        .into_iter()
        .map(|oid| targets.remove(&oid).expect("reached").into_window(oid, max_keys))
        .collect()
}

/// `oid`'s target, made (and listed in `order`) on first reach.
fn reach<'a>(targets: &'a mut HashMap<u32, Target>, order: &mut Vec<u32>, oid: u32) -> &'a mut Target {
    targets.entry(oid).or_insert_with(|| {
        order.push(oid);
        Target::default()
    })
}

/// One fan-out target's union: each key's ops in first-seen order.
#[derive(Default)]
struct Target {
    changed: bool,
    order: Vec<Vec<String>>,
    ops: HashMap<Vec<String>, Seen>,
}

#[derive(Default, Clone, Copy)]
struct Seen {
    insert: bool,
    update: bool,
    delete: bool,
}

impl Target {
    fn add(&mut self, w: &RelWindow, keyed: bool) {
        if !keyed || w.changed {
            self.changed = true;
            return;
        }
        for (keys, mark) in [
            (&w.inserts, 0u8),
            (&w.updates, 1u8),
            (&w.deletes, 2u8),
        ] {
            for k in keys {
                let seen = self.ops.entry(k.clone()).or_insert_with(|| {
                    self.order.push(k.clone());
                    Seen::default()
                });
                match mark {
                    0 => seen.insert = true,
                    1 => seen.update = true,
                    _ => seen.delete = true,
                }
            }
        }
    }
    fn into_window(self, rel: u32, max_keys: usize) -> RelWindow {
        if self.changed || self.order.len() > max_keys {
            return RelWindow {
                rel,
                changed: true,
                ..RelWindow::default()
            };
        }
        let mut w = RelWindow {
            rel,
            ..RelWindow::default()
        };
        for k in self.order {
            let s = self.ops[&k];
            // One op as it is; a key under several (a row moving between partitions: a delete
            // from one and an insert into another) is an update (re-read).
            match (s.insert, s.update, s.delete) {
                (true, false, false) => w.inserts.push(k),
                (false, false, true) => w.deletes.push(k),
                _ => w.updates.push(k),
            }
        }
        w
    }
}

/// What a join of a relation of this `relkind` watches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    /// Keys (an ordinary table without inheritance children, or a partitioned table).
    Keys,
    /// Keyless `changed` only (a legacy inheritance parent).
    Changed,
    /// `live: false, reason: "not_a_table"`.
    NotATable,
}

/// `relkind` is `pg_class.relkind`; `inheritance_parent`: the relation has `pg_inherits`
/// children and isn't partitioned.
pub fn join_kind(relkind: char, inheritance_parent: bool) -> JoinKind {
    match relkind {
        'p' => JoinKind::Keys,
        'r' if inheritance_parent => JoinKind::Changed,
        'r' => JoinKind::Keys,
        _ => JoinKind::NotATable,
    }
}
