//! Resolutions and the merged model `M = T + applied changes + resolutions`.
//!
//! Each object of `M` is the picked side's version (see [`super::integrity::final_entries`]),
//! rewritten into the target's terms: an object the target has keeps the target's OID (a
//! column its `attnum`); an object only the source has gets [`NEW_OID`], and a new column the
//! next `attnum` after the target table's last one, in the source's order. References (owners,
//! grantees, FK targets, owned-by columns, enum types, memberships) point at `M`'s objects, by
//! `M`'s OIDs and names, whichever side the referencing object came from. A reference to an
//! object outside the model (a role or a type the model doesn't cover) keeps its OID only when
//! it's the target's own or a built-in one (below 16384); a source OID becomes [`NEW_OID`], and
//! such references **must be matched by name downstream** (the planner, the drift compare).
//! Expression and definition texts are kept as the picked side prints them (never rewritten; a
//! text naming something `M` calls differently is a dependency conflict, E12a).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::classify::enum_keeps_target_labels;
use super::index::Ctx;
use super::index::name_of;
use super::integrity::{Chosen, Pick, broken, final_entries, referenced_item};
use super::{
    Analysis, Change, Choice, Conflict, ConflictId, ConflictKind, GrantObject, Item, ItemId,
    MergeError, MergedModel, NEW_OID, ObjKey, ObjKind, ObjSnapshot, Outcome, Resolution, World,
};
use crate::model::{
    Column, Constraint, DefaultPrivilege, EnumLabel, EnumType, ForeignKeyTarget, Grant, Identity,
    Index, Membership, NamedRole, ObjClass, Oid, OwnedBy, Policy, Role, RoleRef, Schema,
    SchemaModel, Sequence, Table, membership_merges,
};
use crate::plan::OWNER_ROLE;

fn invalid(c: &Conflict, reason: &str) -> MergeError {
    MergeError::InvalidChoice {
        conflict: c.id.clone(),
        reason: reason.to_string(),
    }
}

fn validate(w: &World, a: &Analysis, c: &Conflict, choice: &Choice) -> Result<(), MergeError> {
    match (c.kind, choice) {
        // E12b (the spec): an order conflict is resolved by the target's columns first or own
        // SQL only.
        (ConflictKind::ColumnOrder, Choice::TakeSource | Choice::KeepTarget) => Err(invalid(
            c,
            "Postgres can't reorder columns without rebuilding the table: take the target's \
             columns first, or write your own SQL",
        )),
        (kind, Choice::TargetColumnsFirst) if kind != ConflictKind::ColumnOrder => Err(invalid(
            c,
            "target columns first only resolves a column order conflict",
        )),
        (_, Choice::OwnSql(sql)) if sql.trim().is_empty() => Err(invalid(c, "own SQL is empty")),
        (_, Choice::TakeSource) => {
            let enums = a.items.iter().filter(|i| {
                i.key.kind == ObjKind::Enum && i.outcome == Outcome::Conflict(c.id.clone())
            });
            for i in enums {
                if let (Some(t), Some(s)) = (w.ti.get(&i.id), w.si.get(&i.id))
                    && !enum_keeps_target_labels(t, s)
                {
                    return Err(invalid(
                        c,
                        "Postgres can't drop or reorder enum values: keep the target or write \
                         your own SQL",
                    ));
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The side whose version of an item the merged model holds.
fn pick(item: &Item, choices: &HashMap<&ConflictId, &Choice>) -> Pick {
    match &item.outcome {
        Outcome::Apply => Pick::Source,
        Outcome::TargetOnly | Outcome::Overlap => Pick::Target,
        Outcome::Conflict(c) => match choices.get(c) {
            Some(Choice::TakeSource) => Pick::Source,
            Some(Choice::TargetColumnsFirst) if matches!(item.source, Some(Change::Create(_))) => {
                Pick::Source
            }
            // Keep target; own SQL replaces the item in the plan, so the model keeps the
            // target's version and the plan generates nothing for it.
            _ => Pick::Target,
        },
    }
}

pub(crate) fn merge(
    a: &Analysis,
    resolutions: &[Resolution],
    w: &World,
) -> Result<MergedModel, MergeError> {
    // The last resolution for a conflict wins; resolutions for conflicts that no longer exist
    // are ignored.
    let choices: HashMap<&ConflictId, &Choice> = resolutions
        .iter()
        .map(|r| (&r.conflict, &r.choice))
        .collect();
    let mut unresolved: Vec<ConflictId> = a
        .conflicts
        .iter()
        .filter(|c| !choices.contains_key(&c.id))
        .map(|c| c.id.clone())
        .collect();
    if !unresolved.is_empty() {
        unresolved.sort();
        return Err(MergeError::Unresolved(unresolved));
    }
    for c in &a.conflicts {
        validate(w, a, c, choices[&c.id])?;
    }

    let items: HashMap<&ItemId, &Item> = a.items.iter().map(|i| (&i.id, i)).collect();
    let fin = final_entries(w, |id| {
        items.get(id).map_or(Pick::Target, |i| pick(i, &choices))
    });

    // The chosen versions must still reference only what the merged model has, unless own SQL
    // takes part (the rehearsal checks that).
    let own: BTreeSet<&ItemId> = a
        .items
        .iter()
        .filter(|i| {
            matches!(&i.outcome, Outcome::Conflict(c) if matches!(choices.get(c), Some(Choice::OwnSql(_))))
        })
        .map(|i| &i.id)
        .collect();
    for (from, missing) in broken(w, &fin) {
        if own.contains(&from) || missing.iter().any(|m| own.contains(&referenced_item(m))) {
            continue;
        }
        return Err(MergeError::BrokenReference {
            item: from,
            missing: missing.iter().map(referenced_item).collect(),
        });
    }

    let own: BTreeSet<ItemId> = own.into_iter().cloned().collect();
    let mut asm = Asm::new(w, &fin);
    asm.own = own;
    let model = asm.model()?;
    let follows_rename = asm.follows_rename();
    let own_sql = a
        .conflicts
        .iter()
        .filter_map(|c| match choices[&c.id] {
            Choice::OwnSql(sql) => Some((c.id.clone(), sql.clone())),
            _ => None,
        })
        .collect();
    Ok(MergedModel {
        model,
        own_sql,
        follows_rename,
    })
}

/// The name a rename changes in a definition text: the object's own (unqualified) name.
fn plain_name(s: &ObjSnapshot) -> String {
    match s {
        ObjSnapshot::Table(x) => x.name.clone(),
        ObjSnapshot::Enum(x) => x.name.clone(),
        ObjSnapshot::Sequence(x) => x.name.clone(),
        other => name_of(other),
    }
}

/// An object's own OID in a snapshot (columns and grants have none).
fn own_oid(s: &ObjSnapshot) -> Option<Oid> {
    match s {
        ObjSnapshot::Schema(x) => Some(x.oid),
        ObjSnapshot::Table(x) => Some(x.oid),
        ObjSnapshot::Constraint { constraint, .. } => Some(constraint.oid),
        ObjSnapshot::Index { index, .. } => Some(index.oid),
        ObjSnapshot::Policy { policy, .. } => Some(policy.oid),
        ObjSnapshot::Enum(x) => Some(x.oid),
        ObjSnapshot::Sequence(x) => Some(x.oid),
        ObjSnapshot::Role(x) => Some(x.oid),
        ObjSnapshot::DefaultPrivilege(x) => Some(x.oid),
        ObjSnapshot::Column { .. } | ObjSnapshot::Grant { .. } => None,
    }
}

/// Builds the merged model from the chosen entries.
struct Asm<'w, 'a> {
    w: &'w World<'a>,
    fin: &'w BTreeMap<ItemId, Chosen<'w>>,
    children: BTreeMap<&'w ItemId, Vec<&'w ItemId>>,
    attnums: HashMap<&'w ItemId, i16>,
    /// (referencing id, column id) for column references `M` can't resolve.
    errors: RefCell<Vec<(ItemId, ItemId)>>,
    /// Items own SQL replaces: a reference from or to one isn't an error (T14 M2, as in
    /// [`broken`]'s check; the rehearsal checks them).
    own: BTreeSet<ItemId>,
}

/// Postgres's `FirstNormalObjectId`: OIDs below it are built in, the same in every database.
const FIRST_NORMAL_OID: u32 = 16384;

impl<'w, 'a> Asm<'w, 'a> {
    fn new(w: &'w World<'a>, fin: &'w BTreeMap<ItemId, Chosen<'w>>) -> Self {
        let mut children: BTreeMap<&ItemId, Vec<&ItemId>> = BTreeMap::new();
        for (id, c) in fin {
            if let Some(p) = &c.entry.parent {
                children.entry(p).or_default().push(id);
            }
        }
        let mut asm = Asm {
            w,
            fin,
            children,
            attnums: HashMap::new(),
            errors: RefCell::new(Vec::new()),
            own: BTreeSet::new(),
        };
        asm.assign_attnums();
        asm
    }

    /// Target columns keep their `attnum`; the rest follow the target table's last one, in the
    /// order of the side they come from.
    fn assign_attnums(&mut self) {
        let mut attnums = HashMap::new();
        for (tid, c) in self.fin {
            if !matches!(c.entry.snap, ObjSnapshot::Table(_)) {
                continue;
            }
            let last = match self.w.ti.get(tid).map(|e| &e.snap) {
                Some(ObjSnapshot::Table(t)) => self
                    .w
                    .target
                    .table(t.oid)
                    .and_then(|t| t.columns.iter().map(|c| c.attnum).max())
                    .unwrap_or(0),
                _ => 0,
            };
            let mut new: Vec<(i16, &ItemId)> = Vec::new();
            for cid in self.children.get(tid).into_iter().flatten() {
                let ObjSnapshot::Column { column, .. } = &self.fin[*cid].entry.snap else {
                    continue;
                };
                match self.w.ti.get(cid).map(|e| &e.snap) {
                    Some(ObjSnapshot::Column { column: t, .. }) => {
                        attnums.insert(*cid, t.attnum);
                    }
                    _ => new.push((column.attnum, *cid)),
                }
            }
            new.sort();
            for (k, (_, cid)) in new.into_iter().enumerate() {
                let k = i16::try_from(k).unwrap_or(i16::MAX - last);
                attnums.insert(cid, last.saturating_add(1).saturating_add(k));
            }
        }
        self.attnums = attnums;
    }

    fn ctx(&self, p: Pick) -> &'w Ctx<'a> {
        match p {
            Pick::Target => &self.w.target,
            Pick::Source => &self.w.source,
        }
    }

    /// The target's OID for an object it has, else [`NEW_OID`].
    fn m_oid(&self, id: &ItemId) -> Oid {
        self.w
            .ti
            .get(id)
            .and_then(|e| own_oid(&e.snap))
            .unwrap_or(NEW_OID)
    }

    fn snap(&self, id: &ItemId) -> Option<&'w ObjSnapshot> {
        self.fin.get(id).map(|c| &c.entry.snap)
    }

    fn role(&self, c: &Ctx, r: &NamedRole) -> NamedRole {
        if let Some(id) = c.role_id(r.oid)
            && let Some(ObjSnapshot::Role(role)) = self.snap(&id)
        {
            return NamedRole {
                oid: self.m_oid(&id),
                name: role.name.clone(),
            };
        }
        NamedRole {
            oid: self.outside(c, r.oid),
            name: r.name.clone(),
        }
    }

    fn role_ref(&self, c: &Ctx, r: &RoleRef) -> RoleRef {
        match r {
            RoleRef::Public => RoleRef::Public,
            RoleRef::Role { oid, name } => {
                let n = self.role(
                    c,
                    &NamedRole {
                        oid: *oid,
                        name: name.clone(),
                    },
                );
                RoleRef::Role {
                    oid: n.oid,
                    name: n.name,
                }
            }
        }
    }

    fn grant(&self, c: &Ctx, g: &Grant) -> Grant {
        Grant {
            grantor: self.role(c, &g.grantor),
            grantee: self.role_ref(c, &g.grantee),
            privilege: g.privilege.clone(),
            grantable: g.grantable,
        }
    }

    /// The grants among an object's children whose target matches `on`.
    fn grants_of(&self, parent: &ItemId, on: impl Fn(&GrantObject) -> bool) -> Vec<Grant> {
        self.children
            .get(parent)
            .into_iter()
            .flatten()
            .filter_map(|id| {
                let chosen = &self.fin[*id];
                match &chosen.entry.snap {
                    ObjSnapshot::Grant { on: o, grant } if on(o) => {
                        Some(self.grant(self.ctx(chosen.pick), grant))
                    }
                    _ => None,
                }
            })
            .collect()
    }

    fn schema_name(&self, c: &Ctx, name: &str) -> String {
        match self.snap(&c.schema_id(name)) {
            Some(ObjSnapshot::Schema(s)) => s.name.clone(),
            _ => name.to_string(),
        }
    }

    /// A table reference as `M` names it: (OID, schema, name).
    fn table_ref(&self, c: &Ctx, oid: Oid, schema: &str, name: &str) -> (Oid, String, String) {
        if c.has_table(oid) {
            let id = c.table_id(oid, schema, name);
            if let Some(chosen) = self.fin.get(&id)
                && let ObjSnapshot::Table(t) = &chosen.entry.snap
            {
                let schema = self.schema_name(self.ctx(chosen.pick), &t.schema);
                return (self.m_oid(&id), schema, t.name.clone());
            }
        }
        (self.outside(c, oid), schema.to_string(), name.to_string())
    }

    fn column_ref(&self, c: &Ctx, table_oid: Oid, attnum: i16) -> Option<&'w ItemId> {
        let t = c.table(table_oid)?;
        let id = c.column_id(table_oid, attnum, &t.schema, &t.name, "");
        self.fin.get_key_value(&id).map(|(k, _)| k)
    }

    /// A column reference's `attnum` in `M`. Never guessed: a column `M` doesn't have is
    /// recorded as a broken reference of `from` (the integrity check normally rules it out).
    fn attnum(&self, from: &ItemId, c: &Ctx, table_oid: Oid, attnum: i16) -> i16 {
        match self
            .column_ref(c, table_oid, attnum)
            .and_then(|id| self.attnums.get(id).copied())
        {
            Some(a) => a,
            None => {
                let missing = c.table(table_oid).map_or_else(
                    || ItemId(format!("column:{}:{attnum}", table_oid.0)),
                    |t| c.column_id(table_oid, attnum, &t.schema, &t.name, ""),
                );
                self.errors.borrow_mut().push((from.clone(), missing));
                0
            }
        }
    }

    /// An OID of an object outside the model (a role or type the model doesn't cover): the
    /// target's own OIDs and built-in OIDs (below `FirstNormalObjectId`, the same in every
    /// database) are kept; another side's OID means nothing on the target, so it's
    /// [`NEW_OID`] and the reference is matched by name downstream.
    fn outside(&self, c: &Ctx, oid: Oid) -> Oid {
        if c.who == self.w.target.who || oid.0 < FIRST_NORMAL_OID {
            oid
        } else {
            NEW_OID
        }
    }

    fn type_oid(&self, c: &Ctx, oid: Oid) -> Oid {
        match c.enum_id(oid) {
            Some(id) if self.fin.contains_key(&id) => self.m_oid(&id),
            _ => self.outside(c, oid),
        }
    }

    fn model(&self) -> Result<SchemaModel, MergeError> {
        let target = self.w.target.model;
        let mut m = SchemaModel::empty(target.server_major);
        m.model_version = target.model_version;
        for (id, chosen) in self.fin {
            let c = self.ctx(chosen.pick);
            match &chosen.entry.snap {
                ObjSnapshot::Schema(s) => m.schemas.push(Schema {
                    oid: self.m_oid(id),
                    name: s.name.clone(),
                    owner: self.role(c, &s.owner),
                    comment: s.comment.clone(),
                    grants: self.grants_of(id, |o| matches!(o, GrantObject::Schema { .. })),
                }),
                ObjSnapshot::Table(t) => m.tables.push(self.table(id, c, t)),
                ObjSnapshot::Enum(e) => m.enums.push(self.enum_type(id, chosen.pick, c, e)),
                ObjSnapshot::Sequence(s) => m.sequences.push(self.sequence(id, c, s)),
                ObjSnapshot::Role(r) => m.roles.push(self.merged_role(id, c, r)),
                ObjSnapshot::DefaultPrivilege(dp) => m.default_privileges.push(DefaultPrivilege {
                    oid: self.m_oid(id),
                    role: self.role(c, &dp.role),
                    schema: dp.schema.as_deref().map(|s| self.schema_name(c, s)),
                    object_type: dp.object_type,
                    grants: dp.grants.iter().map(|g| self.grant(c, g)).collect(),
                }),
                // Assembled with their table or object.
                ObjSnapshot::Column { .. }
                | ObjSnapshot::Constraint { .. }
                | ObjSnapshot::Index { .. }
                | ObjSnapshot::Policy { .. }
                | ObjSnapshot::Grant { .. } => {}
            }
        }
        m.reported = target.reported.clone();
        m.normalize();
        let errors: Vec<(ItemId, ItemId)> = self
            .errors
            .borrow()
            .iter()
            .filter(|(from, missing)| {
                !self.own.contains(from) && !self.own.contains(&referenced_item(missing))
            })
            .cloned()
            .collect();
        if let Some((item, _)) = errors.first() {
            let missing = errors
                .iter()
                .filter(|(from, _)| from == item)
                .map(|(_, m)| m.clone())
                .collect();
            return Err(MergeError::BrokenReference {
                item: item.clone(),
                missing,
            });
        }
        Ok(m)
    }

    /// A role of `M`: the picked side's U2 parts (name, attributes, memberships the roles path
    /// makes, by role), the target's for everything else (E30: other attributes, the comment,
    /// memberships the roles path refuses, a kept membership's own rows and options; for a role
    /// the target doesn't have, none of those). The owner is the target's, whole (E22, E30).
    fn merged_role(&self, id: &ItemId, c: &Ctx, r: &Role) -> Role {
        let target = &self.w.target;
        let t = self.w.ti.get(id).and_then(|e| match &e.snap {
            ObjSnapshot::Role(t) => Some(t),
            _ => None,
        });
        let in_m = |cx: &Ctx, ms: &Membership| {
            let mut ms = ms.clone();
            ms.role = self.role(cx, &ms.role);
            ms.grantor = self.role(cx, &ms.grantor);
            ms
        };
        if r.name == OWNER_ROLE
            && let Some(t) = t
        {
            // Its memberships name roles as `M` does (a role the merge renames, which the
            // owner administers, under its new name).
            return Role {
                oid: self.m_oid(id),
                member_of: t.member_of.iter().map(|ms| in_m(target, ms)).collect(),
                ..t.clone()
            };
        }
        let mut member_of: Vec<Membership> = Vec::new();
        if r.name != OWNER_ROLE {
            let t_rows: Vec<Membership> = t
                .map(|t| t.member_of.iter().map(|ms| in_m(target, ms)).collect())
                .unwrap_or_default();
            member_of.extend(
                t_rows
                    .iter()
                    .filter(|ms| !membership_merges(&ms.role.name))
                    .cloned(),
            );
            let mut seen: BTreeSet<String> = BTreeSet::new();
            for ms in r.member_of.iter().filter(|ms| membership_merges(&ms.role.name)) {
                let ms = in_m(c, ms);
                if !seen.insert(ms.role.name.clone()) {
                    if !t_rows.iter().any(|x| x.role.name == ms.role.name) {
                        member_of.push(ms);
                    }
                    continue;
                }
                let kept: Vec<Membership> = t_rows
                    .iter()
                    .filter(|x| x.role.name == ms.role.name)
                    .cloned()
                    .collect();
                if kept.is_empty() {
                    member_of.push(ms);
                } else {
                    member_of.extend(kept);
                }
            }
        }
        Role {
            oid: self.m_oid(id),
            member_of,
            other_attributes: t.map(|t| t.other_attributes.clone()).unwrap_or_default(),
            comment: t.and_then(|t| t.comment.clone()),
            ..r.clone()
        }
    }

    /// E31: the target's objects whose definition text names an object `M` calls differently
    /// than the target (the merge renames it), by their keys in `M`.
    fn follows_rename(&self) -> Vec<super::FollowsRename> {
        let mut out: Vec<super::FollowsRename> = Vec::new();
        for chosen in self.fin.values() {
            if chosen.pick != Pick::Target {
                continue;
            }
            let mut renames: Vec<super::NameChange> = chosen
                .entry
                .text_refs
                .iter()
                .filter_map(|r| match (self.w.ti.get(r), self.fin.get(r)) {
                    (Some(here), Some(merged))
                        if name_of(&here.snap) != name_of(&merged.entry.snap) =>
                    {
                        Some(super::NameChange {
                            from: plain_name(&here.snap),
                            to: plain_name(&merged.entry.snap),
                        })
                    }
                    _ => None,
                })
                .collect();
            if renames.is_empty() {
                continue;
            }
            renames.retain(|n| n.from != n.to);
            renames.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
            renames.dedup();
            let Some(table) = chosen.entry.parent.as_ref().and_then(|p| self.fin.get(p)) else {
                continue;
            };
            let ObjSnapshot::Table(t) = &table.entry.snap else {
                continue;
            };
            out.push(super::FollowsRename {
                key: ObjKey {
                    kind: chosen.entry.key.kind,
                    schema: Some(self.schema_name(self.ctx(table.pick), &t.schema)),
                    table: Some(t.name.clone()),
                    name: chosen.entry.key.name.clone(),
                },
                renames,
            });
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        out.dedup();
        out
    }

    fn table(&self, id: &ItemId, c: &Ctx, t: &Table) -> Table {
        let mut out = Table {
            oid: self.m_oid(id),
            schema: self.schema_name(c, &t.schema),
            owner: self.role(c, &t.owner),
            grants: self.grants_of(id, |o| matches!(o, GrantObject::Table { .. })),
            ..t.shallow()
        };
        for child in self.children.get(id).into_iter().flatten() {
            let chosen = &self.fin[*child];
            let cc = self.ctx(chosen.pick);
            match &chosen.entry.snap {
                ObjSnapshot::Column {
                    table_oid, column, ..
                } => out.columns.push(self.column(child, cc, *table_oid, column)),
                ObjSnapshot::Constraint {
                    table_oid,
                    constraint,
                    ..
                } => out
                    .constraints
                    .push(self.constraint(child, cc, *table_oid, constraint)),
                ObjSnapshot::Index { index, .. } => out.indexes.push(Index {
                    oid: self.m_oid(child),
                    // T14 I4: the planner builds a source index valid (an invalid one is never
                    // merged).
                    valid: index.valid || chosen.pick == Pick::Source,
                    ..index.clone()
                }),
                ObjSnapshot::Policy { policy, .. } => out.policies.push(Policy {
                    oid: self.m_oid(child),
                    roles: policy.roles.iter().map(|r| self.role_ref(cc, r)).collect(),
                    ..policy.clone()
                }),
                _ => {}
            }
        }
        out
    }

    fn column(&self, id: &ItemId, c: &Ctx, _table_oid: Oid, col: &Column) -> Column {
        let target_identity = match self.w.ti.get(id).map(|e| &e.snap) {
            Some(ObjSnapshot::Column { column, .. }) => {
                column.identity.as_ref().map(|i| i.sequence_oid)
            }
            _ => None,
        };
        Column {
            attnum: self.attnums.get(id).copied().unwrap_or(col.attnum),
            type_oid: self.type_oid(c, col.type_oid),
            identity: col.identity.as_ref().map(|i| Identity {
                sequence_oid: target_identity.unwrap_or(NEW_OID),
                grants: self.grants_of(id, |o| matches!(o, GrantObject::Sequence { .. })),
                ..i.clone()
            }),
            grants: self.grants_of(id, |o| matches!(o, GrantObject::Column { .. })),
            ..col.clone()
        }
    }

    fn constraint(&self, id: &ItemId, c: &Ctx, table_oid: Oid, k: &Constraint) -> Constraint {
        Constraint {
            oid: self.m_oid(id),
            columns: k
                .columns
                .iter()
                .map(|a| self.attnum(id, c, table_oid, *a))
                .collect(),
            references: k.references.as_ref().map(|r| {
                let (oid, schema, table) = self.table_ref(c, r.table_oid, &r.schema, &r.table);
                ForeignKeyTarget {
                    table_oid: oid,
                    schema,
                    table,
                    columns: r
                        .columns
                        .iter()
                        .map(|a| self.attnum(id, c, r.table_oid, *a))
                        .collect(),
                }
            }),
            ..k.clone()
        }
    }

    fn enum_type(&self, id: &ItemId, p: Pick, c: &Ctx, e: &EnumType) -> EnumType {
        let target_side = self.w.dir.target_side();
        EnumType {
            oid: self.m_oid(id),
            schema: self.schema_name(c, &e.schema),
            owner: self.role(c, &e.owner),
            labels: e
                .labels
                .iter()
                .map(|l| EnumLabel {
                    oid: match p {
                        Pick::Target => l.oid,
                        // A value the target has keeps its OID (renamed or not).
                        Pick::Source => c
                            .base_of(ObjClass::EnumLabel, l.oid, None)
                            .and_then(|(b, _)| {
                                self.w.target.map.from_base(
                                    target_side,
                                    ObjClass::EnumLabel,
                                    b,
                                    None,
                                )
                            })
                            .map_or(NEW_OID, |(o, _)| o),
                    },
                    label: l.label.clone(),
                })
                .collect(),
            ..e.clone()
        }
    }

    fn sequence(&self, id: &ItemId, c: &Ctx, s: &Sequence) -> Sequence {
        Sequence {
            oid: self.m_oid(id),
            schema: self.schema_name(c, &s.schema),
            owner: self.role(c, &s.owner),
            owned_by: s.owned_by.as_ref().map(|o| {
                let (table_oid, schema, table) =
                    self.table_ref(c, o.table_oid, &o.schema, &o.table);
                let column = self
                    .column_ref(c, o.table_oid, o.attnum)
                    .and_then(|cid| match self.snap(cid) {
                        Some(ObjSnapshot::Column { column, .. }) => Some(column.name.clone()),
                        _ => None,
                    })
                    .unwrap_or_else(|| o.column.clone());
                OwnedBy {
                    table_oid,
                    schema,
                    table,
                    attnum: self.attnum(id, c, o.table_oid, o.attnum),
                    column,
                }
            }),
            grants: self.grants_of(id, |o| matches!(o, GrantObject::Sequence { .. })),
            ..s.shallow()
        }
    }
}
