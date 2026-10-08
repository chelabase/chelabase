//! Per-item outcomes (the spec's Part 3 table), conflict groups and the flags.
//!
//! Groups beyond one item:
//! - **A container dropped on one side** (a schema or a table) while the other side edits it or
//!   anything in it (a new column or index, an altered policy, …): one `DropVsEdit` conflict, id
//!   the container's id, holding the container and every changed item inside it, so one
//!   resolution decides the whole subtree.
//! - **Both sides adding columns to one table**: a `ColumnOrder` conflict `order:<table id>`
//!   holding the source's new columns and the target's.
//! - **An enum the source changed in a way Postgres can't apply** (a value the target has is gone
//!   or moved): an `Edit` conflict even when the target didn't touch it, and take source is
//!   refused for it (values can only be added or renamed).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::index::Entry;
use super::{
    Change, Conflict, ConflictId, ConflictKind, Item, ItemId, ObjKind, ObjSnapshot, Outcome, World,
};

/// Items and conflicts while the analysis is built, by id.
#[derive(Default)]
pub(crate) struct Draft {
    pub items: BTreeMap<ItemId, Item>,
    pub conflicts: BTreeMap<ConflictId, Conflict>,
}

/// One side's change to an object, from the base entry and the side's entry.
fn change(base: Option<&Entry>, side: Option<&Entry>) -> Option<Change> {
    match (base, side) {
        (None, None) => None,
        (Some(b), None) => Some(Change::Drop(b.snap.clone())),
        (None, Some(x)) => Some(Change::Create(x.snap.clone())),
        (Some(b), Some(x)) => (b.canon != x.canon).then(|| Change::Alter {
            before: Box::new(b.snap.clone()),
            after: Box::new(x.snap.clone()),
        }),
    }
}

/// An index left INVALID (an interrupted concurrent build).
pub(crate) fn invalid_index(e: &Entry) -> bool {
    matches!(&e.snap, ObjSnapshot::Index { index, .. } if !index.valid)
}

/// The same change on both sides: both drops, or creates/alters to the same canonical result.
fn same(s: &Change, t: &Change, se: Option<&Entry>, te: Option<&Entry>) -> bool {
    match (s, t) {
        (Change::Drop(_), Change::Drop(_)) => true,
        (Change::Create(_), Change::Create(_)) | (Change::Alter { .. }, Change::Alter { .. }) => {
            matches!((se, te), (Some(a), Some(b)) if a.canon == b.canon)
        }
        _ => false,
    }
}

fn conflict_kind(s: &Change, t: &Change) -> ConflictKind {
    match (s, t) {
        (Change::Drop(_), Change::Alter { .. }) | (Change::Alter { .. }, Change::Drop(_)) => {
            ConflictKind::DropVsEdit
        }
        (Change::Create(_), Change::Create(_)) => ConflictKind::CreateCollision,
        _ => ConflictKind::Edit,
    }
}

/// Every changed item with its outcome, then the multi-item groups.
pub(crate) fn classify(w: &World) -> Draft {
    let ids: BTreeSet<&ItemId> =
        w.bi.entries
            .keys()
            .chain(w.ti.entries.keys())
            .chain(w.si.entries.keys())
            .collect();
    let mut d = Draft::default();
    for id in ids {
        let (b, t, s) = (w.bi.get(id), w.ti.get(id), w.si.get(id));
        // T14 I4: an index the source left INVALID is no change of the source's (it's reported,
        // `integrity::invalid_indexes`).
        let sc = if s.is_some_and(invalid_index) {
            None
        } else {
            change(b, s)
        };
        let tc = change(b, t);
        let outcome = match (&sc, &tc) {
            (None, None) => continue,
            (Some(_), None) => Outcome::Apply,
            (None, Some(_)) => Outcome::TargetOnly,
            (Some(x), Some(y)) if same(x, y, s, t) => Outcome::Overlap,
            (Some(x), Some(y)) => {
                let cid = ConflictId(id.0.clone());
                let (base, target, source) = w.snaps(id);
                d.conflicts.insert(
                    cid.clone(),
                    Conflict {
                        id: cid.clone(),
                        kind: conflict_kind(x, y),
                        items: vec![id.clone()],
                        base,
                        target,
                        source,
                    },
                );
                Outcome::Conflict(cid)
            }
        };
        let key = s.or(t).or(b).map(|e| e.key.clone()).expect("a side has it");
        d.items.insert(
            id.clone(),
            Item {
                id: id.clone(),
                key,
                source: sc,
                target: tc,
                outcome,
                data_loss: false,
                security: false,
            },
        );
    }
    enum_rule(w, &mut d);
    container_groups(w, &mut d);
    column_order(w, &mut d);
    d
}

/// Makes `members` (and every item of a conflict they were in) one conflict `id`. `listed`
/// names further items the conflict is about without taking them over (a dependency on an item
/// that another conflict already decides).
pub(crate) fn regroup(
    w: &World,
    d: &mut Draft,
    id: ConflictId,
    kind: ConflictKind,
    members: BTreeSet<ItemId>,
    listed: Vec<ItemId>,
    about: &ItemId,
) {
    let mut all = members;
    loop {
        let old: BTreeSet<ConflictId> = all
            .iter()
            .filter_map(|m| match &d.items[m].outcome {
                Outcome::Conflict(c) if *c != id => Some(c.clone()),
                _ => None,
            })
            .collect();
        if old.is_empty() {
            break;
        }
        for c in old {
            if let Some(conflict) = d.conflicts.remove(&c) {
                all.extend(
                    conflict
                        .items
                        .into_iter()
                        .filter(|i| d.items[i].outcome == Outcome::Conflict(c.clone())),
                );
            }
        }
        for m in &all {
            d.items.get_mut(m).expect("an item").outcome = Outcome::Conflict(id.clone());
        }
    }
    for m in &all {
        d.items.get_mut(m).expect("an item").outcome = Outcome::Conflict(id.clone());
    }
    let mut items: BTreeSet<ItemId> = all;
    items.extend(listed);
    // Joining an existing conflict keeps what it is about.
    let (base, target, source) = match d.conflicts.remove(&id) {
        Some(existing) => {
            items.extend(existing.items);
            (existing.base, existing.target, existing.source)
        }
        None => w.snaps(about),
    };
    d.conflicts.insert(
        id.clone(),
        Conflict {
            id,
            kind,
            items: items.into_iter().collect(),
            base,
            target,
            source,
        },
    );
}

/// An enum's values as comparable tokens: the base identity, or the label for a newer value.
fn label_tokens(e: &Entry) -> Vec<String> {
    match &e.canon {
        ObjSnapshot::Enum(en) => en
            .labels
            .iter()
            .map(|l| {
                if l.oid.0 != 0 {
                    format!("#{}", l.oid.0)
                } else {
                    l.label.clone()
                }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Postgres can add and rename enum values, never drop or reorder them: the source's version
/// is reachable from the target's only when every target value is still there, in order.
pub(crate) fn enum_keeps_target_labels(target: &Entry, source: &Entry) -> bool {
    let s = label_tokens(source);
    let mut it = s.iter();
    label_tokens(target)
        .iter()
        .all(|t| it.by_ref().any(|x| x == t))
}

fn enum_rule(w: &World, d: &mut Draft) {
    let bad: Vec<ItemId> = d
        .items
        .values()
        .filter(|i| i.key.kind == ObjKind::Enum && i.outcome == Outcome::Apply)
        .filter(|i| matches!(i.source, Some(Change::Alter { .. })))
        .filter(|i| match (w.ti.get(&i.id), w.si.get(&i.id)) {
            (Some(t), Some(s)) => !enum_keeps_target_labels(t, s),
            _ => false,
        })
        .map(|i| i.id.clone())
        .collect();
    for id in bad {
        regroup(
            w,
            d,
            ConflictId(id.0.clone()),
            ConflictKind::Edit,
            BTreeSet::from([id.clone()]),
            Vec::new(),
            &id,
        );
    }
}

fn ancestors(w: &World, id: &ItemId) -> Vec<ItemId> {
    let mut out = Vec::new();
    let mut cur = w.parent_of(id);
    while let Some(p) = cur {
        if out.contains(p) {
            break;
        }
        out.push(p.clone());
        cur = w.parent_of(p);
    }
    out
}

fn container_groups(w: &World, d: &mut Draft) {
    let anc: HashMap<ItemId, Vec<ItemId>> = d
        .items
        .keys()
        .map(|id| (id.clone(), ancestors(w, id)))
        .collect();
    // (depth, id, the source drops it)
    let mut containers: Vec<(usize, ItemId, bool)> = d
        .items
        .values()
        .filter_map(|i| {
            let s_drop = matches!(i.source, Some(Change::Drop(_)));
            let t_drop = matches!(i.target, Some(Change::Drop(_)));
            (s_drop != t_drop).then(|| (anc[&i.id].len(), i.id.clone(), s_drop))
        })
        .collect();
    containers.sort();
    let mut grouped: BTreeSet<ItemId> = BTreeSet::new();
    for (_, cid, source_drops) in containers {
        if grouped.contains(&cid) {
            continue;
        }
        let other = |it: &Item| -> Option<Change> {
            if source_drops {
                it.target.clone()
            } else {
                it.source.clone()
            }
        };
        let inside: Vec<ItemId> = anc
            .iter()
            .filter(|(_, a)| a.contains(&cid))
            .map(|(id, _)| id.clone())
            .collect();
        let edited = matches!(other(&d.items[&cid]), Some(Change::Alter { .. }))
            || inside.iter().any(|i| {
                matches!(
                    other(&d.items[i]),
                    Some(Change::Create(_) | Change::Alter { .. })
                )
            });
        if !edited {
            continue;
        }
        let mut members: BTreeSet<ItemId> = inside
            .into_iter()
            .filter(|i| d.items[i].outcome != Outcome::Overlap)
            .collect();
        members.insert(cid.clone());
        grouped.extend(members.iter().cloned());
        regroup(
            w,
            d,
            ConflictId(cid.0.clone()),
            ConflictKind::DropVsEdit,
            members,
            Vec::new(),
            &cid,
        );
    }
}

fn column_order(w: &World, d: &mut Draft) {
    let mut by_table: BTreeMap<ItemId, (Vec<ItemId>, Vec<ItemId>)> = BTreeMap::new();
    for i in d.items.values().filter(|i| i.key.kind == ObjKind::Column) {
        let Some(table) = w.parent_of(&i.id) else {
            continue;
        };
        let slot = by_table.entry(table.clone()).or_default();
        match (&i.outcome, &i.source, &i.target) {
            (Outcome::Apply, Some(Change::Create(_)), _) => slot.0.push(i.id.clone()),
            (Outcome::TargetOnly, _, Some(Change::Create(_))) => slot.1.push(i.id.clone()),
            _ => {}
        }
    }
    for (table, (src, tgt)) in by_table {
        if src.is_empty() || tgt.is_empty() {
            continue;
        }
        let members: BTreeSet<ItemId> = src.into_iter().chain(tgt).collect();
        regroup(
            w,
            d,
            ConflictId(format!("order:{}", table.0)),
            ConflictKind::ColumnOrder,
            members,
            Vec::new(),
            &table,
        );
    }
}

/// `data_loss` and `security` for every item whose source version may be merged (applied, or
/// in a conflict where take source would merge it): they describe the step from the target's
/// version to the source's.
pub(crate) fn flags(w: &World, d: &mut Draft) {
    let order: BTreeSet<ConflictId> = d
        .conflicts
        .values()
        .filter(|c| c.kind == ConflictKind::ColumnOrder)
        .map(|c| c.id.clone())
        .collect();
    for item in d.items.values_mut() {
        let applies = match &item.outcome {
            Outcome::Apply => true,
            // Take source isn't offered on an order conflict; its items lose nothing.
            Outcome::Conflict(c) => !order.contains(c),
            Outcome::TargetOnly | Outcome::Overlap => false,
        };
        if !applies {
            continue;
        }
        let (t, s) = (w.ti.get(&item.id), w.si.get(&item.id));
        item.data_loss = loses_data(t, s);
        item.security = gains_privilege(t, s);
    }
}

/// Dropping a table or a column, or changing a column's type other than by a known-safe
/// widening. Dropping NOT NULL never loses data.
fn loses_data(t: Option<&Entry>, s: Option<&Entry>) -> bool {
    match (t, s) {
        (Some(t), None) => matches!(t.snap, ObjSnapshot::Table(_) | ObjSnapshot::Column { .. }),
        (Some(t), Some(s)) => match (&t.canon, &s.canon) {
            (ObjSnapshot::Column { column: a, .. }, ObjSnapshot::Column { column: b, .. }) => {
                !safe_widening(&a.data_type, &b.data_type)
            }
            _ => false,
        },
        _ => false,
    }
}

/// A role created with BYPASSRLS or owner-level, or gaining either (P9, E28).
fn gains_privilege(t: Option<&Entry>, s: Option<&Entry>) -> bool {
    let has = |e: Option<&Entry>, f: fn(&crate::model::Role) -> bool| {
        matches!(e.map(|e| &e.snap), Some(ObjSnapshot::Role(r)) if f(r))
    };
    let bypass = |r: &crate::model::Role| r.bypass_rls;
    let owner_level = |r: &crate::model::Role| r.owner_level();
    (has(s, bypass) && !has(t, bypass)) || (has(s, owner_level) && !has(t, owner_level))
}

/// `format_type` text split into the type name and its modifier (`numeric(10,2)` →
/// `("numeric", Some("10,2"))`).
fn split_typmod(s: &str) -> (&str, Option<&str>) {
    match (s.find('('), s.strip_suffix(')')) {
        (Some(i), Some(body)) => (&s[..i], Some(&body[i + 1..])),
        _ => (s, None),
    }
}

/// A larger-or-equal length limit; no limit takes anything.
fn length_widens(from: Option<&str>, to: Option<&str>) -> bool {
    match (from, to) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(a), Some(b)) => {
            matches!((a.parse::<u32>(), b.parse::<u32>()), (Ok(a), Ok(b)) if b >= a)
        }
    }
}

fn numeric_widens(from: Option<&str>, to: Option<&str>) -> bool {
    let parse = |m: &str| -> Option<(u32, u32)> {
        let mut parts = m.split(',');
        let p = parts.next()?.trim().parse().ok()?;
        let s = parts.next().map_or(Some(0), |s| s.trim().parse().ok())?;
        Some((p, s))
    };
    match (from, to) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(a), Some(b)) => match (parse(a), parse(b)) {
            // Same scale, at least as many digits.
            (Some((p1, s1)), Some((p2, s2))) => s1 == s2 && p2 >= p1,
            _ => false,
        },
    }
}

/// A type change that keeps every value as it is (on `format_type` text): `varchar(n)` →
/// `varchar(m ≥ n)`, `varchar` or `text`; `text` ↔ `varchar`; `smallint` → `integer` →
/// `bigint`; `numeric(p,s)` → `numeric(q ≥ p,s)` or `numeric`; `real` → `double precision`;
/// `varbit(n)` → `varbit(m ≥ n)`; element-wise for arrays.
pub(crate) fn safe_widening(from: &str, to: &str) -> bool {
    if from == to {
        return true;
    }
    match (from.strip_suffix("[]"), to.strip_suffix("[]")) {
        (Some(f), Some(t)) => return safe_widening(f, t),
        (None, None) => {}
        _ => return false,
    }
    let int_rank = |s: &str| match s {
        "smallint" => Some(1),
        "integer" => Some(2),
        "bigint" => Some(3),
        _ => None,
    };
    if let (Some(a), Some(b)) = (int_rank(from), int_rank(to)) {
        return b >= a;
    }
    if from == "real" && to == "double precision" {
        return true;
    }
    let (fb, fm) = split_typmod(from);
    let (tb, tm) = split_typmod(to);
    match (fb, tb) {
        ("character varying", "character varying") | ("bit varying", "bit varying") => {
            length_widens(fm, tm)
        }
        ("character varying", "text") => true,
        ("text", "character varying") => tm.is_none(),
        ("numeric", "numeric") => numeric_widens(fm, tm),
        _ => false,
    }
}
