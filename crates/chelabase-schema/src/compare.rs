//! The expected model after apply and the drift compare (T4).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::analysis::{
    Analysis, Change, FollowsRename, GrantObject, MergedModel, NameChange, ObjKey, ObjKind,
    ObjSnapshot,
};
use crate::model::{
    Column, DefaultPrivilegeObject, Grant, NamedRole, Oid, RoleRef, SchemaModel, Table,
    membership_merges, owner_grants,
};
use crate::plan::{OWNER_ROLE, default_grants, grantee_name, rename_identifiers};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftKind {
    /// Expected but not on the target.
    Missing,
    /// On the target but not expected.
    Unexpected,
    /// On both, different.
    Different,
}

/// One difference between the expected model and the target after apply.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Drift {
    pub key: ObjKey,
    pub kind: DriftKind,
    pub expected: Option<ObjSnapshot>,
    pub actual: Option<ObjSnapshot>,
    /// Produced by own SQL: shown for confirmation, not as drift.
    pub produced: bool,
}

/// The model the target should have after apply: the merged model, plus the grants the
/// default privileges in `default_acl` (role `postgres`, global and per-schema; the data API's
/// rows included, which the stored model excludes) give the tables, identity sequences and
/// sequences the merge creates (P7). New objects are those the merged model marks with OID 0
/// (T3's `NEW_OID`) or that the target doesn't have. The target's roles that the merged model
/// leaves out are kept, since the planner never drops a role. The owner ([`OWNER_ROLE`]) gains
/// Postgres's automatic memberships ([`owner_grants`]) in each role the merged model has and
/// the target doesn't, which phase 0 creates in the owner's session (E22).
pub fn expected_model(
    target: &SchemaModel,
    merged: &MergedModel,
    default_acl: &SchemaModel,
) -> SchemaModel {
    let defaults = &default_acl.default_privileges;
    let mut e = merged.model.clone();
    for t in &mut e.tables {
        let existing = target.tables.iter().find(|x| {
            (t.oid != Oid(0) && x.oid == t.oid) || (x.schema == t.schema && x.name == t.name)
        });
        if existing.is_none() {
            let add = default_grants(defaults, &t.schema, DefaultPrivilegeObject::Tables, &t.owner);
            merge_grants(&mut t.grants, add);
        }
        for c in &mut t.columns {
            let Some(id) = &mut c.identity else { continue };
            let had_identity = existing.is_some_and(|x| {
                x.columns
                    .iter()
                    .any(|xc| xc.name == c.name && xc.identity.is_some())
            });
            if !had_identity {
                let add =
                    default_grants(defaults, &t.schema, DefaultPrivilegeObject::Sequences, &t.owner);
                merge_grants(&mut id.grants, add);
            }
        }
    }
    for q in &mut e.sequences {
        let existing = target.sequences.iter().any(|x| {
            (q.oid != Oid(0) && x.oid == q.oid) || (x.schema == q.schema && x.name == q.name)
        });
        if !existing {
            let add = default_grants(defaults, &q.schema, DefaultPrivilegeObject::Sequences, &q.owner);
            merge_grants(&mut q.grants, add);
        }
    }
    // Roles are never dropped by a merge (a role the merged model leaves out is reported):
    // the target's stay. A role the merged model has under the target's OID is that role,
    // renamed or not (E29).
    let same = |m: &crate::model::Role, t: &crate::model::Role| {
        m.name == t.name || (m.oid != Oid(0) && m.oid == t.oid)
    };
    for r in &target.roles {
        if !e.roles.iter().any(|x| same(x, r)) {
            e.roles.push(r.clone());
        }
    }
    // E22: each role phase 0 creates (in the owner's session) gives the owner Postgres's
    // automatic memberships in it.
    let created: Vec<NamedRole> = e
        .roles
        .iter()
        .filter(|r| r.name != OWNER_ROLE && !target.roles.iter().any(|t| same(r, t)))
        .map(|r| NamedRole {
            oid: r.oid,
            name: r.name.clone(),
        })
        .collect();
    if let Some(owner) = e.roles.iter_mut().find(|r| r.name == OWNER_ROLE) {
        let me = NamedRole {
            oid: owner.oid,
            name: owner.name.clone(),
        };
        for role in &created {
            for g in owner_grants(role, &me) {
                if !owner.member_of.contains(&g) {
                    owner.member_of.push(g);
                }
            }
        }
    }
    e.normalize();
    e
}

fn merge_grants(grants: &mut Vec<Grant>, add: Vec<Grant>) {
    for g in add {
        let same = |x: &Grant| {
            grantee_name(&x.grantee) == grantee_name(&g.grantee) && x.privilege == g.privilege
        };
        match grants.iter_mut().find(|x| same(x)) {
            Some(x) => x.grantable |= g.grantable,
            None => grants.push(g),
        }
    }
}

/// The keys of the items own SQL replaced (the conflicts in the merged model's `own_sql`), as
/// the analysis names them and as each side's snapshot names them, for [`compare`].
pub fn own_sql_keys(analysis: &Analysis, merged: &MergedModel) -> Vec<ObjKey> {
    let mut out: Vec<ObjKey> = Vec::new();
    let mut push = |k: ObjKey| {
        if !out.contains(&k) {
            out.push(k);
        }
    };
    for (conflict, _) in &merged.own_sql {
        let Some(c) = analysis.conflicts.iter().find(|c| &c.id == conflict) else {
            continue;
        };
        for id in &c.items {
            let Some(item) = analysis.items.iter().find(|i| &i.id == id) else {
                continue;
            };
            push(item.key.clone());
            for change in [&item.source, &item.target].into_iter().flatten() {
                let snaps: Vec<&ObjSnapshot> = match change {
                    Change::Create(s) | Change::Drop(s) => vec![s],
                    Change::Alter { before, after } => vec![before, after],
                };
                for s in snaps {
                    if let Some(k) = snapshot_key(s) {
                        push(k);
                    }
                }
            }
        }
    }
    out
}

/// E31: `expected` with each object `keys` names (a column, constraint, index or policy, as
/// [`MergedModel::follows_rename`] lists them) replaced by the rehearsal's version of it, or
/// left out when the rehearsal doesn't have it. The worker compares the rehearsal with these
/// keys marked produced (`compare(expected, rehearsal, keys)`, shown for confirmation), then
/// stores this model as the plan's expected one, so the apply is checked against what the
/// rehearsal produced: Postgres rewrites the text of such an object when the rename it names
/// runs, so `M`'s text (the target's) isn't what the target reads back.
pub fn with_rehearsed(
    expected: &SchemaModel,
    follows: &[FollowsRename],
    rehearsal: &SchemaModel,
) -> SchemaModel {
    let mut out = expected.clone();
    for f in follows {
        let k = &f.key;
        let (Some(schema), Some(table)) = (&k.schema, &k.table) else {
            continue;
        };
        let find = |m: &SchemaModel| -> Option<usize> {
            m.tables
                .iter()
                .position(|t| &t.schema == schema && &t.name == table)
        };
        let Some(ti) = find(&out) else { continue };
        let r = find(rehearsal).map(|i| &rehearsal.tables[i]);
        let renames: HashMap<String, String> = f
            .renames
            .iter()
            .map(|NameChange { from, to }| (from.clone(), to.clone()))
            .collect();
        let rn = |s: &str| rename_identifiers(s, &renames);
        let rn_opt = |s: &Option<String>| s.as_deref().map(rn);
        let t = &mut out.tables[ti];
        let name = k.name.as_str();
        match k.kind {
            ObjKind::Column => {
                if let Some(e) = t.columns.iter_mut().find(|c| c.name == name) {
                    e.data_type = rn(&e.data_type);
                    e.default = rn_opt(&e.default);
                    e.generated = rn_opt(&e.generated);
                    let got = r.and_then(|r| r.columns.iter().find(|c| c.name == name));
                    if let Some(got) = got.filter(|g| canon_column(g) == canon_column(e)) {
                        *e = Column {
                            grants: e.grants.clone(),
                            ..got.clone()
                        };
                    }
                }
            }
            ObjKind::Constraint => {
                if let Some(e) = t.constraints.iter_mut().find(|c| c.name == name) {
                    e.definition = rn(&e.definition);
                    let got = r.and_then(|r| r.constraints.iter().find(|c| c.name == name));
                    if let Some(got) = got.filter(|g| canon_constraint(g) == canon_constraint(e)) {
                        *e = got.clone();
                    }
                }
            }
            ObjKind::Index => {
                if let Some(e) = t.indexes.iter_mut().find(|c| c.name == name) {
                    e.keys = e.keys.iter().map(|x| rn(x)).collect();
                    e.include = e.include.iter().map(|x| rn(x)).collect();
                    e.predicate = rn_opt(&e.predicate);
                    let got = r.and_then(|r| r.indexes.iter().find(|c| c.name == name));
                    if let Some(got) = got.filter(|g| canon_index(g) == canon_index(e)) {
                        *e = got.clone();
                    }
                }
            }
            ObjKind::Policy => {
                if let Some(e) = t.policies.iter_mut().find(|c| c.name == name) {
                    e.using = rn_opt(&e.using);
                    e.with_check = rn_opt(&e.with_check);
                    let got = r.and_then(|r| r.policies.iter().find(|c| c.name == name));
                    if let Some(got) = got.filter(|g| canon_policy(g) == canon_policy(e)) {
                        *e = got.clone();
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn canon_constraint(k: &crate::model::Constraint) -> String {
    format!(
        "{:?}",
        (k.kind, &k.definition, k.deferrable, k.initially_deferred, k.validated)
    )
}

fn canon_index(i: &crate::model::Index) -> String {
    format!(
        "{:?}",
        (
            &i.method,
            i.unique,
            i.nulls_not_distinct,
            &i.keys,
            &i.include,
            &i.predicate,
            &i.options,
            i.valid,
            &i.comment
        )
    )
}

fn canon_policy(p: &crate::model::Policy) -> String {
    format!(
        "{:?}",
        (
            p.command,
            p.permissive,
            role_names(&p.roles),
            &p.using,
            &p.with_check,
            &p.comment
        )
    )
}

fn obj_key(kind: ObjKind, schema: Option<&str>, table: Option<&str>, name: &str) -> ObjKey {
    ObjKey {
        kind,
        schema: schema.map(str::to_string),
        table: table.map(str::to_string),
        name: name.to_string(),
    }
}

/// A snapshot's key (grants are named by their item's key only).
fn snapshot_key(s: &ObjSnapshot) -> Option<ObjKey> {
    Some(match s {
        ObjSnapshot::Schema(x) => obj_key(ObjKind::Schema, None, None, &x.name),
        ObjSnapshot::Table(t) => obj_key(ObjKind::Table, Some(&t.schema), None, &t.name),
        ObjSnapshot::Column {
            schema,
            table,
            column,
            ..
        } => obj_key(ObjKind::Column, Some(schema), Some(table), &column.name),
        ObjSnapshot::Constraint {
            schema,
            table,
            constraint,
            ..
        } => obj_key(ObjKind::Constraint, Some(schema), Some(table), &constraint.name),
        ObjSnapshot::Index {
            schema,
            table,
            index,
            ..
        } => obj_key(ObjKind::Index, Some(schema), Some(table), &index.name),
        ObjSnapshot::Policy {
            schema,
            table,
            policy,
            ..
        } => obj_key(ObjKind::Policy, Some(schema), Some(table), &policy.name),
        ObjSnapshot::Enum(e) => obj_key(ObjKind::Enum, Some(&e.schema), None, &e.name),
        ObjSnapshot::Sequence(q) => obj_key(ObjKind::Sequence, Some(&q.schema), None, &q.name),
        ObjSnapshot::Role(r) => obj_key(ObjKind::Role, None, None, &r.name),
        ObjSnapshot::Grant { .. } | ObjSnapshot::DefaultPrivilege(_) => return None,
    })
}

/// Whether a drift is on an object own SQL replaced (or inside a table it replaced).
fn produced(key: &ObjKey, own: &[ObjKey]) -> bool {
    own.iter().any(|o| {
        o == key
            || (o.kind == ObjKind::Table
                && key.schema == o.schema
                && key.table.as_deref() == Some(o.name.as_str()))
    })
}

struct Cmp<'o> {
    own: &'o [ObjKey],
    out: Vec<Drift>,
}

impl Cmp<'_> {
    fn push(&mut self, key: ObjKey, expected: Option<ObjSnapshot>, actual: Option<ObjSnapshot>) {
        let kind = match (&expected, &actual) {
            (Some(_), None) => DriftKind::Missing,
            (None, Some(_)) => DriftKind::Unexpected,
            _ => DriftKind::Different,
        };
        let produced = produced(&key, self.own);
        self.out.push(Drift {
            key,
            kind,
            expected,
            actual,
            produced,
        });
    }

    /// Matches `e` and `a` by `name`, compares `canon` (OID-free), pushes the differences.
    fn list<T>(
        &mut self,
        e: &[T],
        a: &[T],
        name: impl Fn(&T) -> String,
        canon: impl Fn(&T) -> String,
        key: impl Fn(&T) -> ObjKey,
        snap: impl Fn(&T) -> ObjSnapshot,
    ) {
        for x in e {
            match a.iter().find(|y| name(y) == name(x)) {
                None => self.push(key(x), Some(snap(x)), None),
                Some(y) if canon(x) != canon(y) => self.push(key(x), Some(snap(x)), Some(snap(y))),
                Some(_) => {}
            }
        }
        for y in a.iter().filter(|y| !e.iter().any(|x| name(x) == name(y))) {
            self.push(key(y), None, Some(snap(y)));
        }
    }

    /// Grants on one object, by (grantee, privilege); the grantor isn't compared.
    fn grants(
        &mut self,
        e: &[Grant],
        a: &[Grant],
        on: &GrantObject,
        key: &dyn Fn(&Grant) -> ObjKey,
    ) {
        let name = |g: &Grant| format!("{}\u{1f}{}", grantee_name(&g.grantee), g.privilege);
        let snap = |g: &Grant| ObjSnapshot::Grant {
            on: on.clone(),
            grant: g.clone(),
        };
        self.list(e, a, name, |g| g.grantable.to_string(), key, snap);
    }
}

/// What the drift check compares on a role: what the merge makes (U2: the attributes and the
/// memberships the roles path makes, by role) and nothing it reports "apply by hand" (E30: other
/// attributes, the comment, refused memberships, a membership's options and grantor). The owner
/// is never merged: only its memberships, which the expected model simulates exactly (E22), are
/// compared, in full (role, grantor, options; T14 minor 8).
fn role_canon(r: &crate::model::Role) -> String {
    if r.name == OWNER_ROLE {
        let mut rows: Vec<(&str, &str, bool, bool, bool)> = r
            .member_of
            .iter()
            .map(|m| (m.role.name.as_str(), m.grantor.name.as_str(), m.admin, m.inherit, m.set))
            .collect();
        rows.sort_unstable();
        return format!("{rows:?}");
    }
    let mut members: Vec<&str> = r
        .member_of
        .iter()
        .map(|m| m.role.name.as_str())
        .filter(|n| membership_merges(n))
        .collect();
    members.sort_unstable();
    members.dedup();
    format!(
        "{:?}",
        (
            r.login,
            r.inherit,
            r.connection_limit,
            &r.valid_until,
            r.bypass_rls,
            members
        )
    )
}

fn grant_label(g: &Grant) -> String {
    format!("{} to {}", g.privilege, grantee_name(&g.grantee))
}

fn role_names(roles: &[RoleRef]) -> Vec<String> {
    let mut v: Vec<String> = roles.iter().map(|r| grantee_name(r).to_string()).collect();
    v.sort();
    v
}

fn col_snap(t: &Table, col: &Column) -> ObjSnapshot {
    ObjSnapshot::Column {
        table_oid: t.oid,
        schema: t.schema.clone(),
        table: t.name.clone(),
        column: col.clone(),
    }
}

fn canon_column(c: &Column) -> String {
    format!(
        "{:?}",
        (
            &c.data_type,
            &c.collation,
            &c.default,
            c.not_null,
            c.identity
                .as_ref()
                .map(|i| (i.generation, &i.sequence_name, &i.params)),
            &c.generated,
            &c.comment,
        )
    )
}

/// Differences between `expected` and `actual`, matched by name (objects the merge created
/// have new OIDs, columns new attnums); differences on the objects own SQL replaced
/// ([`own_sql_keys`]) are reported with `produced`, for the user to confirm.
pub fn compare(expected: &SchemaModel, actual: &SchemaModel, own_sql: &[ObjKey]) -> Vec<Drift> {
    let mut c = Cmp {
        own: own_sql,
        out: Vec::new(),
    };
    let (e, a) = (expected, actual);

    c.list(
        &e.schemas,
        &a.schemas,
        |s| s.name.clone(),
        |s| format!("{:?}", (&s.owner.name, &s.comment)),
        |s| obj_key(ObjKind::Schema, None, None, &s.name),
        |s| ObjSnapshot::Schema(s.clone()),
    );
    for s in &e.schemas {
        if let Some(y) = a.schemas.iter().find(|y| y.name == s.name) {
            let on = GrantObject::Schema {
                name: s.name.clone(),
            };
            let key = |g: &Grant| {
                obj_key(ObjKind::Grant, None, None, &format!("schema {}: {}", s.name, grant_label(g)))
            };
            c.grants(&s.grants, &y.grants, &on, &key);
        }
    }

    for t in &e.tables {
        let tkey = obj_key(ObjKind::Table, Some(&t.schema), None, &t.name);
        let Some(u) = a.tables.iter().find(|u| u.schema == t.schema && u.name == t.name) else {
            c.push(tkey, Some(ObjSnapshot::table(t)), None);
            continue;
        };
        let canon = |x: &Table| {
            format!(
                "{:?}",
                (&x.owner.name, &x.comment, x.rls_enabled, x.rls_forced, x.unlogged, &x.options)
            )
        };
        if canon(t) != canon(u) {
            c.push(tkey, Some(ObjSnapshot::table(t)), Some(ObjSnapshot::table(u)));
        }
        let (s, n) = (t.schema.as_str(), t.name.as_str());
        // Each side's column snapshot is built from its own table (its own OID).
        for x in &t.columns {
            let key = obj_key(ObjKind::Column, Some(s), Some(n), &x.name);
            match u.columns.iter().find(|y| y.name == x.name) {
                None => c.push(key, Some(col_snap(t, x)), None),
                Some(y) if canon_column(x) != canon_column(y) => {
                    c.push(key, Some(col_snap(t, x)), Some(col_snap(u, y)))
                }
                Some(_) => {}
            }
        }
        for y in u.columns.iter().filter(|y| !t.columns.iter().any(|x| x.name == y.name)) {
            c.push(
                obj_key(ObjKind::Column, Some(s), Some(n), &y.name),
                None,
                Some(col_snap(u, y)),
            );
        }
        c.list(
            &t.constraints,
            &u.constraints,
            |k| k.name.clone(),
            canon_constraint,
            |k| obj_key(ObjKind::Constraint, Some(s), Some(n), &k.name),
            |k| ObjSnapshot::Constraint {
                table_oid: t.oid,
                schema: s.to_string(),
                table: n.to_string(),
                constraint: k.clone(),
            },
        );
        c.list(
            &t.indexes,
            &u.indexes,
            |i| i.name.clone(),
            canon_index,
            |i| obj_key(ObjKind::Index, Some(s), Some(n), &i.name),
            |i| ObjSnapshot::Index {
                table_oid: t.oid,
                schema: s.to_string(),
                table: n.to_string(),
                index: i.clone(),
            },
        );
        c.list(
            &t.policies,
            &u.policies,
            |p| p.name.clone(),
            canon_policy,
            |p| obj_key(ObjKind::Policy, Some(s), Some(n), &p.name),
            |p| ObjSnapshot::Policy {
                table_oid: t.oid,
                schema: s.to_string(),
                table: n.to_string(),
                policy: p.clone(),
            },
        );
        let on = GrantObject::Table {
            schema: s.to_string(),
            name: n.to_string(),
        };
        let key = |g: &Grant| obj_key(ObjKind::Grant, Some(s), Some(n), &grant_label(g));
        c.grants(&t.grants, &u.grants, &on, &key);
        for x in &t.columns {
            let Some(y) = u.columns.iter().find(|y| y.name == x.name) else {
                continue;
            };
            let on = GrantObject::Column {
                schema: s.to_string(),
                table: n.to_string(),
                column: x.name.clone(),
            };
            let key = |g: &Grant| {
                obj_key(ObjKind::Grant, Some(s), Some(n), &format!("{}: {}", x.name, grant_label(g)))
            };
            c.grants(&x.grants, &y.grants, &on, &key);
            if let (Some(xi), Some(yi)) = (&x.identity, &y.identity) {
                let on = GrantObject::Sequence {
                    schema: s.to_string(),
                    name: xi.sequence_name.clone(),
                };
                let key = |g: &Grant| {
                    obj_key(
                        ObjKind::Grant,
                        Some(s),
                        Some(n),
                        &format!("sequence {}: {}", xi.sequence_name, grant_label(g)),
                    )
                };
                c.grants(&xi.grants, &yi.grants, &on, &key);
            }
        }
    }
    for u in a
        .tables
        .iter()
        .filter(|u| !e.tables.iter().any(|t| t.schema == u.schema && t.name == u.name))
    {
        c.push(
            obj_key(ObjKind::Table, Some(&u.schema), None, &u.name),
            None,
            Some(ObjSnapshot::table(u)),
        );
    }

    c.list(
        &e.enums,
        &a.enums,
        |x| format!("{}\u{1f}{}", x.schema, x.name),
        |x| {
            let labels: Vec<&str> = x.labels.iter().map(|l| l.label.as_str()).collect();
            format!("{:?}", (&x.owner.name, &x.comment, labels))
        },
        |x| obj_key(ObjKind::Enum, Some(&x.schema), None, &x.name),
        |x| ObjSnapshot::Enum(x.clone()),
    );
    c.list(
        &e.sequences,
        &a.sequences,
        |x| format!("{}\u{1f}{}", x.schema, x.name),
        |x| {
            let owned = x
                .owned_by
                .as_ref()
                .map(|o| (o.schema.clone(), o.table.clone(), o.column.clone()));
            format!("{:?}", (&x.owner.name, &x.comment, &x.params, owned))
        },
        |x| obj_key(ObjKind::Sequence, Some(&x.schema), None, &x.name),
        |x| ObjSnapshot::Sequence(x.clone()),
    );
    for q in &e.sequences {
        if let Some(y) = a
            .sequences
            .iter()
            .find(|y| y.schema == q.schema && y.name == q.name)
        {
            let on = GrantObject::Sequence {
                schema: q.schema.clone(),
                name: q.name.clone(),
            };
            let key = |g: &Grant| {
                obj_key(
                    ObjKind::Grant,
                    Some(&q.schema),
                    None,
                    &format!("sequence {}: {}", q.name, grant_label(g)),
                )
            };
            c.grants(&q.grants, &y.grants, &on, &key);
        }
    }
    c.list(
        &e.roles,
        &a.roles,
        |r| r.name.clone(),
        role_canon,
        |r| obj_key(ObjKind::Role, None, None, &r.name),
        |r| ObjSnapshot::Role(r.clone()),
    );
    c.list(
        &e.default_privileges,
        &a.default_privileges,
        |d| format!("{}\u{1f}{:?}\u{1f}{:?}", d.role.name, d.schema, d.object_type),
        |d| {
            let mut g: Vec<(String, String, bool)> = d
                .grants
                .iter()
                .map(|g| (grantee_name(&g.grantee).to_string(), g.privilege.clone(), g.grantable))
                .collect();
            g.sort();
            format!("{g:?}")
        },
        |d| {
            obj_key(
                ObjKind::DefaultPrivilege,
                d.schema.as_deref(),
                None,
                &format!("{} {:?}", d.role.name, d.object_type),
            )
        },
        |d| ObjSnapshot::DefaultPrivilege(d.clone()),
    );
    c.out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{Conflict, ConflictId, ConflictKind, ItemId, ObjKind, Outcome};
    use crate::model::*;
    use crate::testkit::*;

    fn identity_col() -> Column {
        let mut id = not_null(col(1, "id", "bigint"));
        id.identity = Some(Identity {
            generation: IdentityGeneration::ByDefault,
            sequence_oid: Oid(301),
            sequence_name: "fresh_id_seq".into(),
            params: seq_params(),
            grants: Vec::new(),
        });
        id
    }

    #[test]
    fn default_privileges_are_simulated_for_new_tables() {
        let anon = rref(402, "anon");
        let authenticated = rref(403, "authenticated");
        let app = rref(400, "app");
        let target = model(vec![table(100, "public", "old", vec![col(1, "a", "integer")])]);
        // The data API's rows (excluded from the stored model) come in through `default_acl`.
        let mut default_acl = model(vec![]);
        default_acl.default_privileges = vec![
            DefaultPrivilege {
                oid: Oid(1),
                role: postgres(),
                schema: Some("public".into()),
                object_type: DefaultPrivilegeObject::Tables,
                grants: vec![
                    grant(anon.clone(), "SELECT"),
                    grant(authenticated.clone(), "SELECT"),
                ],
            },
            DefaultPrivilege {
                oid: Oid(2),
                role: postgres(),
                schema: None,
                object_type: DefaultPrivilegeObject::Sequences,
                grants: vec![grant(anon.clone(), "USAGE")],
            },
            // Another role's defaults don't apply: the merge creates objects as postgres.
            DefaultPrivilege {
                oid: Oid(3),
                role: named(404, "other"),
                schema: None,
                object_type: DefaultPrivilegeObject::Tables,
                grants: vec![grant(app.clone(), "DELETE")],
            },
            // Another schema's defaults don't apply either.
            DefaultPrivilege {
                oid: Oid(4),
                role: postgres(),
                schema: Some("elsewhere".into()),
                object_type: DefaultPrivilegeObject::Tables,
                grants: vec![grant(app.clone(), "UPDATE")],
            },
        ];
        let mut fresh = table(300, "public", "fresh", vec![identity_col()]);
        fresh.grants = vec![grant(app.clone(), "SELECT")];
        let mut m = model(vec![
            table(100, "public", "old", vec![col(1, "a", "integer")]),
            fresh,
        ]);
        m.sequences = vec![sequence(310, "public", "q")];

        let e = expected_model(&target, &merged(m), &default_acl);
        let fresh = e.tables.iter().find(|t| t.name == "fresh").unwrap();
        let grants: Vec<(String, &str)> = fresh
            .grants
            .iter()
            .map(|g| (format!("{:?}", g.grantee), g.privilege.as_str()))
            .collect();
        assert_eq!(grants.len(), 3, "{grants:?}");
        for (who, privilege) in [(&anon, "SELECT"), (&authenticated, "SELECT"), (&app, "SELECT")] {
            assert!(
                fresh
                    .grants
                    .iter()
                    .any(|g| &g.grantee == who && g.privilege == privilege),
                "{who:?} {privilege}"
            );
        }
        let id_grants = &fresh.columns[0].identity.as_ref().unwrap().grants;
        assert_eq!(id_grants.len(), 1);
        assert_eq!((&id_grants[0].grantee, id_grants[0].privilege.as_str()), (&anon, "USAGE"));
        let q = &e.sequences[0];
        assert_eq!(q.grants.len(), 1);
        assert_eq!(q.grants[0].grantee, anon);
        // The table that already existed keeps its own grants.
        let old = e.tables.iter().find(|t| t.name == "old").unwrap();
        assert!(old.grants.is_empty());

        // What Postgres produced (new OIDs, the grants from the default ACL) isn't drift.
        let mut actual = e.clone();
        for t in &mut actual.tables {
            t.oid = Oid(t.oid.0 + 5000);
        }
        actual.sequences[0].oid = Oid(9999);
        assert_eq!(compare(&e, &actual, &[]), Vec::new());
    }

    #[test]
    fn expected_model_keeps_the_targets_roles() {
        // A role the merged model leaves out is reported, never dropped (P5): it stays on the
        // target, so the expected model keeps it and the compare finds no drift.
        let mut target = model(vec![]);
        target.roles = vec![role(401, "readers"), role(402, "old")];
        let mut m = model(vec![]);
        let mut readers = role(401, "readers");
        readers.inherit = false;
        m.roles = vec![readers.clone(), role(0, "app")];

        let e = expected_model(&target, &merged(m), &model(vec![]));
        let names: Vec<&str> = e.roles.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names.len(), 3, "{names:?}");
        for n in ["readers", "old", "app"] {
            assert!(names.contains(&n), "{n} in {names:?}");
        }
        // The merged model's version wins for a role both have.
        assert!(!e.roles.iter().find(|r| r.name == "readers").unwrap().inherit);

        let mut actual = model(vec![]);
        actual.roles = vec![readers, role(402, "old"), role(9000, "app")];
        assert_eq!(compare(&e, &actual, &[]), Vec::new());
    }

    // E22: a role the merge creates (in the owner's session) gives the owner Postgres's two
    // automatic memberships; the expected model has them, so they aren't drift. An owner grant
    // made by hand still is.
    #[test]
    fn expected_model_adds_the_owners_automatic_memberships() {
        let mut target = model(vec![]);
        let mut owner = role(10, "postgres");
        owner.member_of = owner_grants(&named(401, "readers"), &postgres());
        target.roles = vec![owner.clone(), role(401, "readers")];
        let mut m = model(vec![]);
        m.roles = vec![owner, role(401, "readers"), role(0, "app"), role(0, "web")];

        let e = expected_model(&target, &merged(m), &model(vec![]));
        let owner = e.roles.iter().find(|r| r.name == "postgres").unwrap();
        let mut want = owner_grants(&named(401, "readers"), &postgres());
        want.extend(owner_grants(&named(0, "app"), &postgres()));
        want.extend(owner_grants(&named(0, "web"), &postgres()));
        let mut want_role = owner.clone();
        want_role.member_of = want;
        let mut want_model = model(vec![]);
        want_model.roles = vec![want_role];
        want_model.normalize();
        assert_eq!(owner.member_of, want_model.roles[0].member_of);
        // The admin grant comes from the bootstrap superuser, the self-grant from the owner.
        let app: Vec<(&str, bool, bool, bool)> = owner
            .member_of
            .iter()
            .filter(|x| x.role.name == "app")
            .map(|x| (x.grantor.name.as_str(), x.admin, x.inherit, x.set))
            .collect();
        assert_eq!(app, [("cloud_admin", true, false, false), ("postgres", false, true, true)]);

        // What Postgres made on the target (new OIDs) isn't drift.
        let mut actual = e.clone();
        for r in &mut actual.roles {
            if r.oid == Oid(0) {
                r.oid = Oid(9000);
            }
        }
        assert_eq!(compare(&e, &actual, &[]), Vec::new());

        // An owner grant made by hand is drift on the owner.
        let mut by_hand = actual.clone();
        by_hand
            .roles
            .iter_mut()
            .find(|r| r.name == "postgres")
            .unwrap()
            .member_of
            .push(membership(402, "other"));
        let drift = compare(&e, &by_hand, &[]);
        assert_eq!(drift.len(), 1, "{drift:#?}");
        assert_eq!(drift[0].key, key(ObjKind::Role, None, None, "postgres"));
        assert_eq!(drift[0].kind, DriftKind::Different);

        // A role the target already had gives the owner nothing new.
        let e2 = expected_model(&target, &merged(target.clone()), &model(vec![]));
        assert_eq!(e2.roles, {
            let mut t = target.clone();
            t.normalize();
            t.roles
        });
    }

    // T14 I3 / E30, minor 7: what the merge never changes on a role (other attributes, the
    // comment, memberships the roles path refuses, a member's options, the owner's attributes)
    // isn't drift; minor 8: the owner's memberships, which the expected model simulates exactly,
    // are compared in full.
    #[test]
    fn compare_ignores_role_parts_the_merge_never_makes() {
        let mut expected = model(vec![]);
        let mut owner = role(10, "postgres");
        owner.member_of = owner_grants(&named(401, "app"), &postgres());
        let mut app = role(401, "app");
        app.member_of = vec![membership(402, "readers")];
        expected.roles = vec![owner.clone(), app.clone(), role(402, "readers")];
        expected.normalize();

        let mut actual = expected.clone();
        for r in &mut actual.roles {
            if r.name == "app" {
                r.other_attributes = vec!["CREATEDB".into()];
                r.comment = Some("by hand".into());
                r.member_of.push(membership(4200, "pg_read_all_data"));
                r.member_of[0].admin = true;
            }
            if r.name == "postgres" {
                r.connection_limit = 3;
                r.comment = Some("by hand".into());
            }
        }
        assert_eq!(compare(&expected, &actual, &[]), Vec::new());

        // The owner's memberships: an option differs.
        let mut by_hand = expected.clone();
        let owner = by_hand.roles.iter_mut().find(|r| r.name == "postgres").unwrap();
        owner.member_of[1].admin = !owner.member_of[1].admin;
        let drift = compare(&expected, &by_hand, &[]);
        assert_eq!(drift.len(), 1, "{drift:#?}");
        assert_eq!(drift[0].key, key(ObjKind::Role, None, None, "postgres"));

        // A merged attribute still is drift.
        let mut changed = expected.clone();
        changed.roles.iter_mut().find(|r| r.name == "app").unwrap().login = true;
        assert_eq!(compare(&expected, &changed, &[]).len(), 1);
    }

    // T14 I2 (E31), fix round M4: a target object following a rename the merge makes is
    // checked against what the rehearsal produced, but only when that is the expected object
    // with the rename applied: a rehearsal that dropped or changed it shows drift, in the
    // rehearsal and after the apply.
    #[test]
    fn objects_following_a_rename_compare_with_the_rehearsal() {
        let mut t = table(100, "public", "t", vec![col(1, "uid", "integer")]);
        t.policies = vec![policy(200, "own", "(user_id = 1)")];
        let expected = model(vec![t.clone()]);
        let follows = [FollowsRename {
            key: key(ObjKind::Policy, Some("public"), Some("t"), "own"),
            renames: vec![NameChange {
                from: "user_id".into(),
                to: "uid".into(),
            }],
        }];
        let rehearsed = |using: Option<&str>| {
            let mut r = t.clone();
            r.oid = Oid(900);
            match using {
                Some(u) => {
                    r.policies[0].using = Some(u.into());
                    r.policies[0].oid = Oid(901);
                }
                None => r.policies.clear(),
            }
            model(vec![r])
        };

        // A correct rehearsal: no drift, there or after an apply that reads the same.
        let good = rehearsed(Some("(uid = 1)"));
        let checked = with_rehearsed(&expected, &follows, &good);
        assert_eq!(compare(&checked, &good, &[]), Vec::new());
        let mut applied = good.clone();
        applied.tables[0].policies[0].oid = Oid(902);
        assert_eq!(compare(&checked, &applied, &[]), Vec::new());
        // The apply is held to the rehearsal's version.
        let mut off = applied.clone();
        off.tables[0].policies[0].using = Some("(uid = 2)".into());
        assert_eq!(compare(&checked, &off, &[]).len(), 1);

        // A rehearsal that dropped the policy, or changed it beyond the rename: drift.
        for bad in [rehearsed(None), rehearsed(Some("(uid = 2)"))] {
            let checked = with_rehearsed(&expected, &follows, &bad);
            let drift = compare(&checked, &bad, &[]);
            assert_eq!(drift.len(), 1, "{drift:#?}");
            assert!(!drift[0].produced);
            assert_eq!(drift[0].key.name, "own");
        }
        // Nothing listed: the expected model as it was.
        assert_eq!(with_rehearsed(&expected, &[], &good), expected);
    }

    // E29: a role the merge renames is the target's role, under its new name: not kept twice,
    // and not "created" (no automatic owner memberships).
    #[test]
    fn expected_model_follows_role_renames() {
        let mut target = model(vec![]);
        target.roles = vec![role(10, "postgres"), role(401, "a")];
        let mut m = model(vec![]);
        m.roles = vec![role(10, "postgres"), role(401, "b")];
        let e = expected_model(&target, &merged(m), &model(vec![]));
        let names: Vec<&str> = e.roles.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["b", "postgres"]);
        assert!(e.roles.iter().all(|r| r.member_of.is_empty()));
    }

    #[test]
    fn compare_finds_missing_column() {
        let expected = model(vec![table(
            100,
            "public",
            "t",
            vec![col(1, "a", "integer"), col(2, "b", "text")],
        )]);
        // New OIDs and attnums on the target: matched by name.
        let actual = model(vec![table(999, "public", "t", vec![col(5, "a", "integer")])]);
        let drift = compare(&expected, &actual, &[]);
        assert_eq!(drift.len(), 1, "{drift:#?}");
        let d = &drift[0];
        assert_eq!(d.kind, DriftKind::Missing);
        assert_eq!(d.key, key(ObjKind::Column, Some("public"), Some("t"), "b"));
        assert!(d.actual.is_none());
        assert!(matches!(&d.expected, Some(ObjSnapshot::Column { column, .. }) if column.name == "b"));
        assert!(!d.produced);

        // A changed property is `Different`.
        let mut changed = actual.clone();
        changed.tables[0].columns[0].data_type = "bigint".into();
        changed.tables[0].columns.push(col(6, "b", "text"));
        let drift = compare(&expected, &changed, &[]);
        assert_eq!(drift.len(), 1, "{drift:#?}");
        assert_eq!(drift[0].kind, DriftKind::Different);
        assert_eq!(drift[0].key, key(ObjKind::Column, Some("public"), Some("t"), "a"));
    }

    #[test]
    fn compare_ignores_own_sql_items() {
        let expected = model(vec![table(100, "public", "t", vec![col(1, "a", "integer")])]);
        let mut actual = model(vec![
            table(100, "public", "t", vec![col(1, "a", "integer"), col(2, "x", "text")]),
            table(101, "public", "u", vec![]),
        ]);
        actual.tables[0].comment = Some("made by hand".into());

        let mut analysis = no_analysis();
        let mut item = applied(
            "table:100",
            key(ObjKind::Table, Some("public"), None, "t"),
            crate::analysis::Change::Create(ObjSnapshot::table(&expected.tables[0])),
        );
        item.outcome = Outcome::Conflict(ConflictId("c1".into()));
        analysis.items = vec![item];
        analysis.conflicts = vec![Conflict {
            id: ConflictId("c1".into()),
            kind: ConflictKind::Edit,
            items: vec![ItemId("table:100".into())],
            base: None,
            target: None,
            source: None,
        }];
        let mut m = merged(expected.clone());
        m.own_sql = vec![(ConflictId("c1".into()), "ALTER TABLE t ADD x text".into())];
        let own = own_sql_keys(&analysis, &m);
        assert_eq!(own, [key(ObjKind::Table, Some("public"), None, "t")]);

        let drift = compare(&expected, &actual, &own);
        let summary: Vec<(ObjKey, DriftKind, bool)> = drift
            .iter()
            .map(|d| (d.key.clone(), d.kind, d.produced))
            .collect();
        assert_eq!(
            summary,
            [
                (key(ObjKind::Table, Some("public"), None, "t"), DriftKind::Different, true),
                (key(ObjKind::Column, Some("public"), Some("t"), "x"), DriftKind::Unexpected, true),
                (key(ObjKind::Table, Some("public"), None, "u"), DriftKind::Unexpected, false),
            ]
        );
    }
}
