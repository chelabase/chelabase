//! The merged model's integrity: which version of each object it holds, the references that
//! must resolve in it, dependency conflicts, and the source's reported objects (with the merged
//! expressions that call a function only the source has).

use std::collections::{BTreeMap, BTreeSet};

use super::classify::{Draft, regroup};
use super::index::{Entry, identity_id, name_of};
use super::{
    ConflictId, ConflictKind, ItemId, ObjSnapshot, Outcome, ReportedChange, ReportedChangeKind,
    World,
};
use crate::model::{Reported, ReportedKind};

/// Which side's version of an object the merged model holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pick {
    Target,
    Source,
}

/// An object of the merged model: the chosen side's entry.
#[derive(Clone, Copy)]
pub(crate) struct Chosen<'w> {
    pub pick: Pick,
    pub entry: &'w Entry,
}

/// Every object of the merged model, by id: for each object either side has, the picked side's
/// version (absent when that side doesn't have it). Objects that aren't items are unchanged,
/// so the target's version is taken.
pub(crate) fn final_entries<'w>(
    w: &'w World,
    pick: impl Fn(&ItemId) -> Pick,
) -> BTreeMap<ItemId, Chosen<'w>> {
    let ids: BTreeSet<&ItemId> = w.ti.entries.keys().chain(w.si.entries.keys()).collect();
    ids.into_iter()
        .filter_map(|id| {
            let p = pick(id);
            let entry = match p {
                Pick::Target => w.ti.get(id),
                Pick::Source => w.si.get(id),
            }?;
            Some((id.clone(), Chosen { pick: p, entry }))
        })
        .collect()
}

/// The pick before resolutions: applied items from the source, everything else (conflicts
/// included, as if kept) from the target.
pub(crate) fn provisional(d: &Draft, id: &ItemId) -> Pick {
    match d.items.get(id).map(|i| &i.outcome) {
        Some(Outcome::Apply) => Pick::Source,
        _ => Pick::Target,
    }
}

/// References that don't resolve in the merged model: (referencing id, missing ids). An object
/// taken from the source whose definition text names something the merged model calls
/// differently (the target renamed it) counts as missing that thing too: the text isn't
/// rewritten (E12a). Objects taken from the target are already in its database, where Postgres
/// keeps their texts up to date through renames.
pub(crate) fn broken(w: &World, fin: &BTreeMap<ItemId, Chosen>) -> Vec<(ItemId, Vec<ItemId>)> {
    let mut present: BTreeSet<&ItemId> = fin.keys().collect();
    let identities: Vec<ItemId> = fin
        .iter()
        .filter(|(_, c)| {
            matches!(&c.entry.snap, ObjSnapshot::Column { column, .. } if column.identity.is_some())
        })
        .map(|(id, _)| identity_id(id))
        .collect();
    present.extend(identities.iter());
    fin.iter()
        .filter_map(|(id, c)| {
            let mut missing: Vec<ItemId> = c
                .entry
                .refs
                .iter()
                .filter(|r| !present.contains(r))
                .cloned()
                .collect();
            if c.pick == Pick::Source {
                missing.extend(
                    c.entry
                        .text_refs
                        .iter()
                        .filter(|r| match (w.si.get(r), fin.get(*r)) {
                            (Some(here), Some(merged)) => {
                                name_of(&here.snap) != name_of(&merged.entry.snap)
                            }
                            _ => false,
                        })
                        .cloned(),
                );
                missing.sort();
                missing.dedup();
            }
            (!missing.is_empty()).then(|| (id.clone(), missing))
        })
        .collect()
}

/// The item a reference names (an identity pseudo id names its column).
pub(crate) fn referenced_item(id: &ItemId) -> ItemId {
    match id.0.strip_suffix("/identity") {
        Some(column) => ItemId(column.to_string()),
        None => id.clone(),
    }
}

/// Adds a `Dependency` conflict `dependency:<referencing id>` for every reference the merged
/// model (as merged before resolutions) leaves dangling, holding the referencing item and the
/// referenced one (each only when no other conflict decides it already). Repeats until stable:
/// a new conflict keeps the target's versions, which can't break a target reference.
pub(crate) fn add_dependency_conflicts(w: &World, d: &mut Draft) {
    loop {
        let found = {
            let fin = final_entries(w, |id| provisional(d, id));
            broken(w, &fin)
        };
        let mut changed = false;
        for (from, missing) in found {
            // Everything it needs is decided by one other conflict (a grant on a column in a
            // column order conflict, an FK to a column in a drop-vs-edit conflict): it joins
            // that conflict, so one resolution decides both.
            let deciders: BTreeSet<Option<ConflictId>> = missing
                .iter()
                .map(
                    |m| match d.items.get(&referenced_item(m)).map(|i| &i.outcome) {
                        Some(Outcome::Conflict(c)) => Some(c.clone()),
                        _ => None,
                    },
                )
                .collect();
            let from_free = d
                .items
                .get(&from)
                .is_some_and(|i| !matches!(i.outcome, Outcome::Conflict(_)));
            if from_free
                && deciders.len() == 1
                && let Some(Some(c)) = deciders.into_iter().next()
            {
                let kind = d.conflicts[&c].kind;
                regroup(
                    w,
                    d,
                    c,
                    kind,
                    BTreeSet::from([from.clone()]),
                    Vec::new(),
                    &from,
                );
                changed = true;
                continue;
            }
            let mut listed = vec![from.clone()];
            listed.extend(missing.iter().map(referenced_item));
            listed.retain(|i| d.items.contains_key(i));
            let members: BTreeSet<ItemId> = listed
                .iter()
                .filter(|i| !matches!(d.items[*i].outcome, Outcome::Conflict(_)))
                .cloned()
                .collect();
            if members.is_empty() {
                continue;
            }
            regroup(
                w,
                d,
                ConflictId(format!("dependency:{}", from.0)),
                ConflictKind::Dependency,
                members,
                listed,
                &from,
            );
            changed = true;
        }
        if !changed {
            break;
        }
    }
}

type ReportedKey = (
    ReportedKind,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

fn reported_key(r: &Reported) -> ReportedKey {
    (
        r.kind.clone(),
        r.schema.clone(),
        r.name.clone(),
        r.parent.clone(),
        r.arguments.clone(),
    )
}

fn by_key(list: &[Reported]) -> BTreeMap<ReportedKey, &Reported> {
    list.iter()
        .filter(|r| r.kind != ReportedKind::MergeLeftover)
        .map(|r| (reported_key(r), r))
        .collect()
}

/// T14 I4: the indexes the source left INVALID (an interrupted `CREATE INDEX CONCURRENTLY`),
/// which the analysis never merges: `Created` for one the base doesn't have, `Altered` for a
/// base index the source left invalid (an interrupted rebuild). One the base already had invalid
/// isn't a change. By table and name.
pub(crate) fn invalid_indexes(w: &World) -> Vec<ReportedChange> {
    let mut out: Vec<ReportedChange> = Vec::new();
    for (id, e) in &w.si.entries {
        let ObjSnapshot::Index {
            schema,
            table,
            index,
            ..
        } = &e.snap
        else {
            continue;
        };
        if index.valid {
            continue;
        }
        let change = match w.bi.get(id) {
            None => ReportedChangeKind::Created,
            Some(b) if !super::classify::invalid_index(b) => ReportedChangeKind::Altered,
            Some(_) => continue,
        };
        out.push(ReportedChange {
            object: Reported {
                kind: ReportedKind::InvalidIndex,
                oid: index.oid,
                schema: Some(schema.clone()),
                name: index.name.clone(),
                parent: Some(table.clone()),
                arguments: None,
                definition: Some(crate::plan::index_definition(
                    index,
                    &crate::sql::qualified(schema, table),
                )),
            },
            change,
            needed_by: Vec::new(),
        });
    }
    out.sort_by(|a, b| {
        (&a.object.schema, &a.object.parent, &a.object.name).cmp(&(
            &b.object.schema,
            &b.object.parent,
            &b.object.name,
        ))
    });
    out
}

/// The functions an expression statement calls, as `schema.name` or `name`.
fn called_functions(statement: &str) -> Vec<String> {
    pg_query::parse(statement)
        .map(|r| r.functions())
        .unwrap_or_default()
}

fn names_function(call: &str, f: &Reported) -> bool {
    match &f.schema {
        Some(schema) if call.contains('.') => call == format!("{schema}.{}", f.name),
        _ => call == f.name,
    }
}

/// Source-side changes to reported objects (matched by kind, schema, name, parent and
/// arguments; a change the target already has is left out), and for each function a merged
/// expression calls that the target doesn't have, the items that need it.
pub(crate) fn reported(w: &World, d: &Draft) -> Vec<ReportedChange> {
    let base = by_key(&w.base.model.reported);
    let target = by_key(&w.target.model.reported);
    let source = by_key(&w.source.model.reported);
    let same_on_target =
        |k: &ReportedKey, s: &Reported| target.get(k).is_some_and(|t| t.definition == s.definition);
    let mut out: BTreeMap<ReportedKey, ReportedChange> = BTreeMap::new();
    let keys: BTreeSet<&ReportedKey> = base.keys().chain(source.keys()).collect();
    for k in keys {
        let change = match (base.get(k), source.get(k)) {
            (None, Some(s)) if !same_on_target(k, s) => Some((ReportedChangeKind::Created, *s)),
            (Some(b), None) if target.contains_key(k) => Some((ReportedChangeKind::Dropped, *b)),
            (Some(b), Some(s)) if b.definition != s.definition && !same_on_target(k, s) => {
                Some((ReportedChangeKind::Altered, *s))
            }
            _ => None,
        };
        if let Some((change, object)) = change {
            out.insert(
                k.clone(),
                ReportedChange {
                    object: object.clone(),
                    change,
                    needed_by: Vec::new(),
                },
            );
        }
    }

    let functions: Vec<&Reported> = source
        .values()
        .copied()
        .filter(|f| f.kind == ReportedKind::Function)
        // Only the source has it.
        .filter(|f| {
            !target.values().any(|t| {
                t.kind == ReportedKind::Function && t.schema == f.schema && t.name == f.name
            })
        })
        .collect();
    if !functions.is_empty() {
        for item in d.items.values() {
            if !matches!(item.outcome, Outcome::Apply | Outcome::Conflict(_)) {
                continue;
            }
            let Some(e) = w.si.get(&item.id) else {
                continue;
            };
            for statement in &e.exprs {
                for call in called_functions(statement) {
                    for f in functions.iter().filter(|f| names_function(&call, f)) {
                        out.entry(reported_key(f))
                            .or_insert_with(|| ReportedChange {
                                object: (*f).clone(),
                                change: ReportedChangeKind::Unchanged,
                                needed_by: Vec::new(),
                            })
                            .needed_by
                            .push(item.id.clone());
                    }
                }
            }
        }
    }
    let mut out: Vec<ReportedChange> = out.into_values().collect();
    for r in &mut out {
        r.needed_by.sort();
        r.needed_by.dedup();
    }
    out
}
