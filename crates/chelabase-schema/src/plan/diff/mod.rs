//! The two-way diff target → merged model as DDL, each statement tagged with its bucket (its
//! place in the spec's Part 4 order) and phase.
//!
//! **Matching.** The merged model keeps the target's OIDs (columns: the table's OID and the
//! attnum) for the target's objects and gives new ones OID 0 (T3's `NEW_OID`), so a merged
//! object with a target OID is that object, renamed or not. Then the renames the analysis
//! reports (an `Alter` whose name changes) and, last, equal names. References to other objects
//! (FK targets, owners, grantees) are by name only (E12d). An object the analysis reports as
//! dropped and re-created under one name is a drop plus a create. Unmatched target objects are
//! dropped, unmatched merged ones created. Target definition text is read through the table's
//! column renames before it's compared, as Postgres prints it with the current names.
//!
//! **Order** (buckets): roles; schema renames, new schemas; moves and renames (a dropped object
//! holding a name that's needed is first renamed aside); policies, foreign keys and other
//! constraints that go or change; dropped indexes; detached defaults and sequence ownership;
//! dropped columns and tables (referencing tables first); schemas → enums → sequences → tables
//! → columns → sequence ownership → PK/unique/check → foreign keys → indexes → RLS → policies →
//! grants → default privileges; last, dropped sequences, enums and schemas (after the columns
//! that used them changed).
//!
//! **Phases.** Tables that exist on the target (R5) get the online transforms: concurrent
//! indexes, `NOT VALID` + `VALIDATE`, unique constraints `USING INDEX`, NOT NULL through a
//! temporary check. New tables get everything in phase 1.

mod objects;
mod renames;
mod tables;

use std::collections::{HashMap, HashSet};

use super::ddl::{self, OWNER_ROLE};
use super::{Breaking, OnlineCheck, Phase, RoleOp, RolePatchModel, RoleSpecModel, StepKind};
use crate::analysis::{Analysis, Change, ConflictId, GrantObject, ItemId, ObjKind, ObjSnapshot};
use crate::error::SchemaError;
use crate::model::*;
use crate::sql::{ident, literal};

/// A target object's identity: class, OID, sub-id (a column's attnum).
type TRef = (ObjClass, Oid, i16);

/// A statement's place in the order. Statements are stably sorted by bucket, then by phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Bucket {
    Roles,
    SchemaRename,
    SchemaCreate,
    Rename,
    DetachPolicy,
    DetachForeignKey,
    DetachConstraint,
    DropIndex,
    DetachDefault,
    DropColumn,
    DropTable,
    Schema,
    Enum,
    Sequence,
    Table,
    Column,
    OwnedBy,
    Constraint,
    ForeignKey,
    Index,
    Rls,
    Policy,
    Grant,
    DefaultPrivilege,
    DropSequence,
    DropEnum,
    DropSchema,
}

/// An enum value added to an enum the target has: phase 1 unless the plan uses it, then 1a.
pub(super) struct EnumAdd {
    /// The enum, so all of one enum's adds move together.
    pub group: String,
    pub label: String,
    /// The statement with the target's names (phase 1a runs before phase 1's renames).
    pub sql_1a: String,
    /// The target has a value of this name that the plan renames away (T14 minor 11).
    pub renamed_away: bool,
}

/// One generated (or inserted) statement before it becomes a [`super::Step`].
pub(super) struct Out {
    pub bucket: Bucket,
    pub phase: Phase,
    pub kind: StepKind,
    pub id: String,
    pub sql: String,
    /// Planner keys of the objects it implements (linked to analysis items).
    pub keys: Vec<String>,
    /// Items given directly (own SQL).
    pub items: Vec<ItemId>,
    /// Drops a table or column.
    pub drop: bool,
    /// Changes a column's type: data loss unless the analysis says the change is safe.
    pub type_change: bool,
    pub rewrite: bool,
    pub security: bool,
    pub locks: Vec<String>,
    pub role_op: Option<RoleOp>,
    pub online: Option<OnlineCheck>,
    pub breaking: Option<Breaking>,
    pub enum_add: Option<EnumAdd>,
    /// User steps: the step it came from and where it was anchored.
    pub user: Option<UserOrigin>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct UserOrigin {
    pub step: String,
    pub anchor: Option<String>,
}

impl Out {
    pub(super) fn new(
        bucket: Bucket,
        phase: Phase,
        kind: StepKind,
        id: String,
        sql: String,
    ) -> Out {
        Out {
            bucket,
            phase,
            kind,
            id,
            sql,
            keys: Vec::new(),
            items: Vec::new(),
            drop: false,
            type_change: false,
            rewrite: false,
            security: false,
            locks: Vec::new(),
            role_op: None,
            online: None,
            breaking: None,
            enum_add: None,
            user: None,
        }
    }

    fn key(&mut self, k: String) -> &mut Self {
        if !self.keys.contains(&k) {
            self.keys.push(k);
        }
        self
    }

    fn lock(&mut self, mode: &str, object: &str) -> &mut Self {
        self.locks.push(format!("{mode} {object}"));
        self
    }

    fn online(&mut self, check: OnlineCheck) -> &mut Self {
        self.online = Some(check);
        self
    }
}

// ---- planner keys: kind + names, joined with a separator no identifier contains ----

const SEP: char = '\u{1f}';

pub(super) fn pkey(kind: &str, parts: &[&str]) -> String {
    let mut s = kind.to_string();
    for p in parts {
        s.push(SEP);
        s.push_str(p);
    }
    s
}

fn grant_pkey(on: &str, g: &Grant) -> String {
    pkey("grant", &[on, ddl::grantee_name(&g.grantee), &g.privilege])
}

fn on_pkey(on: &GrantObject) -> String {
    match on {
        GrantObject::Schema { name } => pkey("schema", &[name]),
        GrantObject::Table { schema, name } => pkey("table", &[schema, name]),
        GrantObject::Column {
            schema,
            table,
            column,
        } => pkey("column", &[schema, table, column]),
        GrantObject::Sequence { schema, name } => pkey("sequence", &[schema, name]),
    }
}

fn defpriv_pkey(role: &str, schema: Option<&str>, o: DefaultPrivilegeObject) -> String {
    pkey(
        "default_privilege",
        &[role, schema.unwrap_or(""), ddl::default_object(o)],
    )
}

/// The planner keys a snapshot names.
pub(super) fn snapshot_keys(s: &ObjSnapshot) -> Vec<String> {
    let k = match s {
        ObjSnapshot::Schema(x) => pkey("schema", &[&x.name]),
        ObjSnapshot::Table(t) => pkey("table", &[&t.schema, &t.name]),
        ObjSnapshot::Column {
            schema,
            table,
            column,
            ..
        } => pkey("column", &[schema, table, &column.name]),
        ObjSnapshot::Constraint {
            schema,
            table,
            constraint,
            ..
        } => pkey("constraint", &[schema, table, &constraint.name]),
        ObjSnapshot::Index {
            schema,
            table,
            index,
            ..
        } => pkey("index", &[schema, table, &index.name]),
        ObjSnapshot::Policy {
            schema,
            table,
            policy,
            ..
        } => pkey("policy", &[schema, table, &policy.name]),
        ObjSnapshot::Grant { on, grant } => grant_pkey(&on_pkey(on), grant),
        ObjSnapshot::Enum(e) => pkey("enum", &[&e.schema, &e.name]),
        ObjSnapshot::Sequence(q) => pkey("sequence", &[&q.schema, &q.name]),
        ObjSnapshot::Role(r) => pkey("role", &[&r.name]),
        ObjSnapshot::DefaultPrivilege(d) => {
            defpriv_pkey(&d.role.name, d.schema.as_deref(), d.object_type)
        }
    };
    vec![k]
}

pub(super) fn change_snapshots(c: &Change) -> Vec<&ObjSnapshot> {
    match c {
        Change::Create(s) | Change::Drop(s) => vec![s],
        Change::Alter { before, after } => vec![before, after],
    }
}

/// Where own SQL replacing a conflict's items goes: the bucket of the first item's kind.
pub(super) fn bucket_for_conflict(
    analysis: &Analysis,
    conflict: &ConflictId,
) -> (Bucket, Vec<ItemId>) {
    let Some(c) = analysis.conflicts.iter().find(|c| &c.id == conflict) else {
        return (Bucket::Grant, Vec::new());
    };
    let kind = c
        .items
        .first()
        .and_then(|id| analysis.items.iter().find(|i| &i.id == id))
        .map(|i| i.key.kind);
    let bucket = match kind {
        Some(ObjKind::Schema) => Bucket::Schema,
        Some(ObjKind::Table) => Bucket::Table,
        Some(ObjKind::Column) => Bucket::Column,
        Some(ObjKind::Constraint) => Bucket::Constraint,
        Some(ObjKind::Index) => Bucket::Index,
        Some(ObjKind::Policy) => Bucket::Policy,
        Some(ObjKind::Enum) => Bucket::Enum,
        Some(ObjKind::Sequence) => Bucket::Sequence,
        Some(ObjKind::DefaultPrivilege) => Bucket::DefaultPrivilege,
        Some(ObjKind::Grant | ObjKind::Role) | None => Bucket::Grant,
    };
    (bucket, c.items.clone())
}

// ---- matching ----

#[derive(Clone, Default, Debug)]
struct Pairs {
    /// (target index, merged index), in merged order.
    pairs: Vec<(usize, usize)>,
    t_only: Vec<usize>,
    m_only: Vec<usize>,
}

impl Pairs {
    fn m_of(&self, ti: usize) -> Option<usize> {
        self.pairs.iter().find(|p| p.0 == ti).map(|p| p.1)
    }
}

/// The OID the merged model gives an object it creates (T3's `NEW_OID`): never matched.
const NEW_OID: Oid = Oid(0);

/// Same target identity: the merged model keeps the target's OIDs (and attnums) for the
/// target's objects; new objects carry [`NEW_OID`].
fn same_oid(t: Oid, m: Oid) -> bool {
    m != NEW_OID && t == m
}

/// The target's identity first (`identity(ti, mi)`: the merged object keeps the target's
/// OID), then renames the analysis reports (`renamed(ti, mi)`, names differing), then equal
/// keys (unless `skip(mi)`: the merged object is a re-creation).
// Indices are the interface here (the closures take them), not just loop counters.
#[allow(clippy::needless_range_loop)]
fn pair_by(
    t_len: usize,
    m_len: usize,
    tk: impl Fn(usize) -> String,
    mk: impl Fn(usize) -> String,
    skip: impl Fn(usize) -> bool,
    identity: impl Fn(usize, usize) -> bool,
    renamed: impl Fn(usize, usize) -> bool,
) -> Pairs {
    let mut t_used = vec![false; t_len];
    let mut m_used = vec![false; m_len];
    let mut pairs = Vec::new();
    for mi in 0..m_len {
        if skip(mi) {
            continue;
        }
        if let Some(ti) = (0..t_len).find(|&ti| !t_used[ti] && identity(ti, mi)) {
            t_used[ti] = true;
            m_used[mi] = true;
            pairs.push((ti, mi));
        }
    }
    for mi in 0..m_len {
        if m_used[mi] {
            continue;
        }
        let mkey = mk(mi);
        if let Some(ti) = (0..t_len).find(|&ti| !t_used[ti] && tk(ti) != mkey && renamed(ti, mi)) {
            t_used[ti] = true;
            m_used[mi] = true;
            pairs.push((ti, mi));
        }
    }
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    for ti in 0..t_len {
        by_key.entry(tk(ti)).or_default().push(ti);
    }
    for mi in 0..m_len {
        if m_used[mi] || skip(mi) {
            continue;
        }
        if let Some(&ti) = by_key
            .get(&mk(mi))
            .and_then(|l| l.iter().find(|&&ti| !t_used[ti]))
        {
            t_used[ti] = true;
            m_used[mi] = true;
            pairs.push((ti, mi));
        }
    }
    pairs.sort_by_key(|p| p.1);
    Pairs {
        pairs,
        t_only: (0..t_len).filter(|&i| !t_used[i]).collect(),
        m_only: (0..m_len).filter(|&i| !m_used[i]).collect(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Schema,
    Table,
    Column,
    Constraint,
    Index,
    Policy,
    Enum,
    Label,
    Sequence,
}

/// A rename the analysis reports: `before` (base names) became `after` (the side's names).
struct Fact {
    tag: Tag,
    before_parent: Vec<String>,
    before: String,
    after_parent: Vec<String>,
    after: String,
    /// The base OID of the parent (table or enum), when the snapshot has it.
    parent_oid: Option<Oid>,
    /// The base OID of the object itself, when the snapshot has it.
    oid: Option<Oid>,
}

fn facts_of(analysis: &Analysis) -> Vec<Fact> {
    let mut facts = Vec::new();
    for item in &analysis.items {
        for change in [&item.source, &item.target].into_iter().flatten() {
            if let Change::Alter { before, after } = change {
                facts_from(before, after, &mut facts);
            }
        }
    }
    facts
}

fn facts_from(before: &ObjSnapshot, after: &ObjSnapshot, out: &mut Vec<Fact>) {
    let s = |x: &str| x.to_string();
    let mut fact = |tag, bp: Vec<String>, b: &str, ap: Vec<String>, a: &str, po, oid| {
        if b != a || bp != ap {
            out.push(Fact {
                tag,
                before_parent: bp,
                before: b.to_string(),
                after_parent: ap,
                after: a.to_string(),
                parent_oid: po,
                oid,
            });
        }
    };
    match (before, after) {
        (ObjSnapshot::Schema(a), ObjSnapshot::Schema(b)) => fact(
            Tag::Schema,
            vec![],
            &a.name,
            vec![],
            &b.name,
            None,
            Some(a.oid),
        ),
        (ObjSnapshot::Table(a), ObjSnapshot::Table(b)) => fact(
            Tag::Table,
            vec![s(&a.schema)],
            &a.name,
            vec![s(&b.schema)],
            &b.name,
            None,
            Some(a.oid),
        ),
        (
            ObjSnapshot::Column {
                table_oid,
                schema,
                table,
                column: a,
            },
            ObjSnapshot::Column {
                schema: s2,
                table: t2,
                column: b,
                ..
            },
        ) if a.name != b.name => fact(
            Tag::Column,
            vec![s(schema), s(table)],
            &a.name,
            vec![s(s2), s(t2)],
            &b.name,
            Some(*table_oid),
            None,
        ),
        (
            ObjSnapshot::Constraint {
                table_oid,
                schema,
                table,
                constraint: a,
            },
            ObjSnapshot::Constraint {
                schema: s2,
                table: t2,
                constraint: b,
                ..
            },
        ) if a.name != b.name => fact(
            Tag::Constraint,
            vec![s(schema), s(table)],
            &a.name,
            vec![s(s2), s(t2)],
            &b.name,
            Some(*table_oid),
            Some(a.oid),
        ),
        (
            ObjSnapshot::Index {
                table_oid,
                schema,
                table,
                index: a,
            },
            ObjSnapshot::Index {
                schema: s2,
                table: t2,
                index: b,
                ..
            },
        ) if a.name != b.name => fact(
            Tag::Index,
            vec![s(schema), s(table)],
            &a.name,
            vec![s(s2), s(t2)],
            &b.name,
            Some(*table_oid),
            Some(a.oid),
        ),
        (
            ObjSnapshot::Policy {
                table_oid,
                schema,
                table,
                policy: a,
            },
            ObjSnapshot::Policy {
                schema: s2,
                table: t2,
                policy: b,
                ..
            },
        ) if a.name != b.name => fact(
            Tag::Policy,
            vec![s(schema), s(table)],
            &a.name,
            vec![s(s2), s(t2)],
            &b.name,
            Some(*table_oid),
            Some(a.oid),
        ),
        (ObjSnapshot::Enum(a), ObjSnapshot::Enum(b)) => {
            fact(
                Tag::Enum,
                vec![s(&a.schema)],
                &a.name,
                vec![s(&b.schema)],
                &b.name,
                None,
                Some(a.oid),
            );
            for la in &a.labels {
                if let Some(lb) = b
                    .labels
                    .iter()
                    .find(|l| l.oid == la.oid && l.label != la.label)
                {
                    fact(
                        Tag::Label,
                        vec![s(&a.schema), s(&a.name)],
                        &la.label,
                        vec![s(&b.schema), s(&b.name)],
                        &lb.label,
                        Some(a.oid),
                        Some(la.oid),
                    );
                }
            }
        }
        (ObjSnapshot::Sequence(a), ObjSnapshot::Sequence(b)) => fact(
            Tag::Sequence,
            vec![s(&a.schema)],
            &a.name,
            vec![s(&b.schema)],
            &b.name,
            None,
            Some(a.oid),
        ),
        _ => {}
    }
}

/// Keys the analysis reports as dropped and created again under one name (both applied).
fn recreated_of(analysis: &Analysis) -> HashSet<String> {
    let mut dropped = HashSet::new();
    let mut created = HashSet::new();
    for item in &analysis.items {
        if item.outcome != crate::analysis::Outcome::Apply {
            continue;
        }
        match &item.source {
            Some(Change::Drop(s)) => dropped.extend(snapshot_keys(s)),
            Some(Change::Create(s)) => created.extend(snapshot_keys(s)),
            _ => {}
        }
    }
    dropped.intersection(&created).cloned().collect()
}

// ---- the generator ----

struct TablePair {
    ti: usize,
    mi: usize,
    cols: Pairs,
    cons: Pairs,
    idx: Pairs,
    pols: Pairs,
    /// The table's column renames (target name → merged name).
    renames: ColumnRenames,
}

struct Gen<'a> {
    t: &'a SchemaModel,
    m: &'a SchemaModel,
    facts: Vec<Fact>,
    recreated: HashSet<String>,
    out: Vec<Out>,
    ids: HashMap<String, u32>,
    tmp: u32,
    ix: u32,
    /// Suffix of temporary index names, so another merge's leftover never matches.
    merge_id: uuid::Uuid,
    schemas: Pairs,
    /// Target schema name → its name once schema renames ran.
    schema_map: HashMap<String, String>,
    tables: Pairs,
    tps: Vec<TablePair>,
    enums: Pairs,
    labels: Vec<Pairs>,
    seqs: Pairs,
    /// Final names of dropped target objects (renamed aside, or kept).
    cur: HashMap<TRef, String>,
    /// Merged (schema, table, sorted columns) of unique keys built in phase 2.
    online_uniques: HashSet<(String, String, Vec<i16>)>,
}

pub(super) fn generate(
    t: &SchemaModel,
    m: &SchemaModel,
    analysis: &Analysis,
    merge_id: uuid::Uuid,
) -> Result<Vec<Out>, SchemaError> {
    // E29: phase 0 renames roles first; everything else is planned against the target as it
    // reads after those renames (grantees, owners and policy roles under their new names).
    let (role_outs, renamed) = role_steps(t, m)?;
    let t = &t.rename_roles(|oid, _| renamed.get(&oid).cloned());
    let mut g = Gen {
        t,
        m,
        facts: facts_of(analysis),
        recreated: recreated_of(analysis),
        out: Vec::new(),
        ids: HashMap::new(),
        tmp: 0,
        ix: 0,
        merge_id,
        schemas: Pairs::default(),
        schema_map: HashMap::new(),
        tables: Pairs::default(),
        tps: Vec::new(),
        enums: Pairs::default(),
        labels: Vec::new(),
        seqs: Pairs::default(),
        cur: HashMap::new(),
        online_uniques: HashSet::new(),
    };
    g.out = role_outs;
    g.pair_all();
    g.renames()?;
    g.schemas()?;
    g.enum_types()?;
    g.sequences()?;
    g.all_tables()?;
    g.default_privileges()?;
    Ok(g.out)
}

fn owner_changed(t: &NamedRole, m: &NamedRole) -> bool {
    t.name != m.name
}

fn sorted(cols: &[i16]) -> Vec<i16> {
    let mut v = cols.to_vec();
    v.sort_unstable();
    v
}

/// Column renames on one table (target name → merged name): the target's definition text is
/// read through them before it's compared, since Postgres prints definitions with the
/// columns' current names.
type ColumnRenames = HashMap<String, String>;

fn same_text(t: &str, m: &str, renames: &ColumnRenames) -> bool {
    t == m || ddl::rename_identifiers(t, renames) == m
}

fn same_opt_text(t: &Option<String>, m: &Option<String>, renames: &ColumnRenames) -> bool {
    match (t, m) {
        (Some(t), Some(m)) => same_text(t, m, renames),
        (None, None) => true,
        _ => false,
    }
}

fn same_texts(t: &[String], m: &[String], renames: &ColumnRenames) -> bool {
    t.len() == m.len() && t.iter().zip(m).all(|(t, m)| same_text(t, m, renames))
}

fn constraint_changed(t: &Constraint, m: &Constraint, renames: &ColumnRenames) -> bool {
    t.kind != m.kind
        || !same_text(
            ddl::strip_not_valid(&t.definition),
            ddl::strip_not_valid(&m.definition),
            renames,
        )
        || t.deferrable != m.deferrable
        || t.initially_deferred != m.initially_deferred
}

fn index_changed(t: &Index, m: &Index, renames: &ColumnRenames) -> bool {
    t.method != m.method
        || t.unique != m.unique
        || t.nulls_not_distinct != m.nulls_not_distinct
        || !same_texts(&t.keys, &m.keys, renames)
        || !same_texts(&t.include, &m.include, renames)
        || !same_opt_text(&t.predicate, &m.predicate, renames)
        || t.options != m.options
}

fn policy_changed(t: &Policy, m: &Policy, renames: &ColumnRenames) -> bool {
    let names = |p: &Policy| {
        let mut v: Vec<String> = p
            .roles
            .iter()
            .map(|r| ddl::grantee_name(r).to_string())
            .collect();
        v.sort();
        v
    };
    t.command != m.command
        || t.permissive != m.permissive
        || names(t) != names(m)
        || !same_opt_text(&t.using, &m.using, renames)
        || !same_opt_text(&t.with_check, &m.with_check, renames)
}

/// The memberships the roles path makes (U2), without the owner-level group, sorted and
/// deduplicated: a membership it refuses (`pg_*`, reserved) is never planned (E30).
fn memberships(r: &Role) -> Vec<String> {
    let mut v: Vec<String> = r
        .member_of
        .iter()
        .map(|m| m.role.name.clone())
        .filter(|n| n != OWNER_LEVEL_ROLE && membership_merges(n))
        .collect();
    v.sort();
    v.dedup();
    v
}

/// What the target's default privileges (role `postgres`, global and per-schema) grant on a
/// new object of `object` in `schema` (P7).
pub(crate) fn default_grants(
    defaults: &[DefaultPrivilege],
    schema: &str,
    object: DefaultPrivilegeObject,
    owner: &NamedRole,
) -> Vec<Grant> {
    let mut out: Vec<Grant> = Vec::new();
    for d in defaults {
        if d.role.name != OWNER_ROLE || d.object_type != object {
            continue;
        }
        if d.schema.as_deref().is_some_and(|s| s != schema) {
            continue;
        }
        for g in &d.grants {
            let same = |x: &Grant| {
                ddl::grantee_name(&x.grantee) == ddl::grantee_name(&g.grantee)
                    && x.privilege == g.privilege
            };
            match out.iter_mut().find(|x| same(x)) {
                Some(x) => x.grantable |= g.grantable,
                None => out.push(Grant {
                    grantor: owner.clone(),
                    ..g.clone()
                }),
            }
        }
    }
    out
}

/// A grant's sort key: PUBLIC first, then roles by name, then the privilege.
fn grant_sort_key(g: &Grant) -> (u8, String, String) {
    match &g.grantee {
        RoleRef::Public => (0, String::new(), g.privilege.clone()),
        RoleRef::Role { name, .. } => (1, name.clone(), g.privilege.clone()),
    }
}

impl<'a> Gen<'a> {
    fn emit(
        &mut self,
        bucket: Bucket,
        phase: Phase,
        verb: &str,
        path: &[&str],
        sql: String,
    ) -> &mut Out {
        let base = format!("{verb}:{}", path.join("."));
        let n = self.ids.entry(base.clone()).or_insert(0);
        *n += 1;
        let id = if *n == 1 { base } else { format!("{base}#{n}") };
        self.out
            .push(Out::new(bucket, phase, StepKind::Generated, id, sql));
        self.out.last_mut().expect("just pushed")
    }

    fn tx(&mut self, bucket: Bucket, verb: &str, path: &[&str], sql: String) -> &mut Out {
        self.emit(bucket, Phase::Tx, verb, path, sql)
    }

    fn online(&mut self, bucket: Bucket, verb: &str, path: &[&str], sql: String) -> &mut Out {
        self.emit(bucket, Phase::Online, verb, path, sql)
    }

    #[allow(clippy::too_many_arguments)]
    fn renamed(
        &self,
        tag: Tag,
        t_parent: &[&str],
        t_name: &str,
        t_parent_oid: Option<Oid>,
        t_oid: Option<Oid>,
        m_parent: &[&str],
        m_name: &str,
    ) -> bool {
        let eq =
            |a: &[String], b: &[&str]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y);
        self.facts.iter().any(|f| {
            if f.tag != tag || f.after != m_name {
                return false;
            }
            // The target's name is the base name, or (renamed on both sides) the target's own.
            let name_ok = f.before == t_name
                || self.facts.iter().any(|g| {
                    g.tag == tag
                        && g.before == f.before
                        && g.before_parent == f.before_parent
                        && g.after == t_name
                });
            let ctx_ok = eq(&f.before_parent, t_parent)
                || eq(&f.after_parent, m_parent)
                || (f.parent_oid.is_some() && f.parent_oid == t_parent_oid)
                || (f.oid.is_some() && f.oid == t_oid);
            name_ok && ctx_ok
        })
    }

    /// A target schema's name once schema renames ran.
    fn cur_schema(&self, t_schema: &str) -> String {
        self.schema_map
            .get(t_schema)
            .cloned()
            .unwrap_or_else(|| t_schema.to_string())
    }

    fn cur_name(&self, tref: TRef, default: &str) -> String {
        self.cur
            .get(&tref)
            .cloned()
            .unwrap_or_else(|| default.to_string())
    }

    // ---- matching ----

    fn pair_all(&mut self) {
        let (t, m) = (self.t, self.m);
        self.schemas = pair_by(
            t.schemas.len(),
            m.schemas.len(),
            |i| t.schemas[i].name.clone(),
            |i| m.schemas[i].name.clone(),
            |_| false,
            |ti, mi| same_oid(t.schemas[ti].oid, m.schemas[mi].oid),
            |ti, mi| {
                self.renamed(
                    Tag::Schema,
                    &[],
                    &t.schemas[ti].name,
                    None,
                    Some(t.schemas[ti].oid),
                    &[],
                    &m.schemas[mi].name,
                )
            },
        );
        for &(ti, mi) in &self.schemas.pairs {
            self.schema_map
                .insert(t.schemas[ti].name.clone(), m.schemas[mi].name.clone());
        }

        let tables = pair_by(
            t.tables.len(),
            m.tables.len(),
            |i| {
                pkey(
                    "table",
                    &[&self.cur_schema(&t.tables[i].schema), &t.tables[i].name],
                )
            },
            |i| pkey("table", &[&m.tables[i].schema, &m.tables[i].name]),
            |i| {
                self.recreated
                    .contains(&pkey("table", &[&m.tables[i].schema, &m.tables[i].name]))
            },
            |ti, mi| same_oid(t.tables[ti].oid, m.tables[mi].oid),
            |ti, mi| {
                let (a, b) = (&t.tables[ti], &m.tables[mi]);
                self.renamed(
                    Tag::Table,
                    &[&a.schema],
                    &a.name,
                    None,
                    Some(a.oid),
                    &[&b.schema],
                    &b.name,
                ) || self.renamed(
                    Tag::Table,
                    &[&self.cur_schema(&a.schema)],
                    &a.name,
                    None,
                    Some(a.oid),
                    &[&b.schema],
                    &b.name,
                )
            },
        );
        let mut tps = Vec::new();
        for &(ti, mi) in &tables.pairs {
            let (a, b) = (&t.tables[ti], &m.tables[mi]);
            let tp_parent: [&str; 2] = [&a.schema, &a.name];
            let mp_parent: [&str; 2] = [&b.schema, &b.name];
            // Sub-objects keep the target's OIDs (columns: attnums) only when the table does.
            let same_table = same_oid(a.oid, b.oid);
            type Side<'x> = &'x dyn Fn(usize) -> (String, Oid);
            let sub = |tag: Tag, kind: &str, tn: Side, tl: usize, mn: Side, ml: usize| {
                pair_by(
                    tl,
                    ml,
                    |i| tn(i).0,
                    |i| mn(i).0,
                    |i| {
                        self.recreated
                            .contains(&pkey(kind, &[&b.schema, &b.name, &mn(i).0]))
                    },
                    |ti, mi| same_table && same_oid(tn(ti).1, mn(mi).1),
                    |ti, mi| {
                        let oid = (tag != Tag::Column).then(|| tn(ti).1);
                        self.renamed(
                            tag,
                            &tp_parent,
                            &tn(ti).0,
                            Some(a.oid),
                            oid,
                            &mp_parent,
                            &mn(mi).0,
                        )
                    },
                )
            };
            // Columns' identity is the attnum (as an OID-like number; 0 never matches).
            let cols = sub(
                Tag::Column,
                "column",
                &|i| {
                    (
                        a.columns[i].name.clone(),
                        Oid(a.columns[i].attnum.max(0) as u32),
                    )
                },
                a.columns.len(),
                &|i| {
                    (
                        b.columns[i].name.clone(),
                        Oid(b.columns[i].attnum.max(0) as u32),
                    )
                },
                b.columns.len(),
            );
            let cons = sub(
                Tag::Constraint,
                "constraint",
                &|i| (a.constraints[i].name.clone(), a.constraints[i].oid),
                a.constraints.len(),
                &|i| (b.constraints[i].name.clone(), b.constraints[i].oid),
                b.constraints.len(),
            );
            let idx = sub(
                Tag::Index,
                "index",
                &|i| (a.indexes[i].name.clone(), a.indexes[i].oid),
                a.indexes.len(),
                &|i| (b.indexes[i].name.clone(), b.indexes[i].oid),
                b.indexes.len(),
            );
            let pols = sub(
                Tag::Policy,
                "policy",
                &|i| (a.policies[i].name.clone(), a.policies[i].oid),
                a.policies.len(),
                &|i| (b.policies[i].name.clone(), b.policies[i].oid),
                b.policies.len(),
            );
            let renames: ColumnRenames = cols
                .pairs
                .iter()
                .filter(|&&(ct, cm)| a.columns[ct].name != b.columns[cm].name)
                .map(|&(ct, cm)| (a.columns[ct].name.clone(), b.columns[cm].name.clone()))
                .collect();
            tps.push(TablePair {
                ti,
                mi,
                cols,
                cons,
                idx,
                pols,
                renames,
            });
        }
        self.tables = tables;
        self.tps = tps;

        self.enums = pair_by(
            t.enums.len(),
            m.enums.len(),
            |i| {
                pkey(
                    "enum",
                    &[&self.cur_schema(&t.enums[i].schema), &t.enums[i].name],
                )
            },
            |i| pkey("enum", &[&m.enums[i].schema, &m.enums[i].name]),
            |i| {
                self.recreated
                    .contains(&pkey("enum", &[&m.enums[i].schema, &m.enums[i].name]))
            },
            |ti, mi| same_oid(t.enums[ti].oid, m.enums[mi].oid),
            |ti, mi| {
                let (a, b) = (&t.enums[ti], &m.enums[mi]);
                self.renamed(
                    Tag::Enum,
                    &[&a.schema],
                    &a.name,
                    None,
                    Some(a.oid),
                    &[&b.schema],
                    &b.name,
                )
            },
        );
        let mut labels = Vec::new();
        for &(ti, mi) in &self.enums.pairs {
            let (a, b) = (&t.enums[ti], &m.enums[mi]);
            labels.push(pair_by(
                a.labels.len(),
                b.labels.len(),
                |i| a.labels[i].label.clone(),
                |i| b.labels[i].label.clone(),
                |_| false,
                |ti, mi| same_oid(a.oid, b.oid) && same_oid(a.labels[ti].oid, b.labels[mi].oid),
                |ti, mi| {
                    self.renamed(
                        Tag::Label,
                        &[&a.schema, &a.name],
                        &a.labels[ti].label,
                        Some(a.oid),
                        Some(a.labels[ti].oid),
                        &[&b.schema, &b.name],
                        &b.labels[mi].label,
                    )
                },
            ));
        }
        self.labels = labels;

        self.seqs = pair_by(
            t.sequences.len(),
            m.sequences.len(),
            |i| {
                pkey(
                    "sequence",
                    &[
                        &self.cur_schema(&t.sequences[i].schema),
                        &t.sequences[i].name,
                    ],
                )
            },
            |i| pkey("sequence", &[&m.sequences[i].schema, &m.sequences[i].name]),
            |i| {
                self.recreated.contains(&pkey(
                    "sequence",
                    &[&m.sequences[i].schema, &m.sequences[i].name],
                ))
            },
            |ti, mi| same_oid(t.sequences[ti].oid, m.sequences[mi].oid),
            |ti, mi| {
                let (a, b) = (&t.sequences[ti], &m.sequences[mi]);
                self.renamed(
                    Tag::Sequence,
                    &[&a.schema],
                    &a.name,
                    None,
                    Some(a.oid),
                    &[&b.schema],
                    &b.name,
                )
            },
        );

        // Unique keys built online on tables the target has: foreign keys to them wait there.
        let mut online = HashSet::new();
        for tp in &self.tps {
            let (a, b) = (&t.tables[tp.ti], &m.tables[tp.mi]);
            let new_or_changed_con = |mi: usize| {
                tp.cons.m_only.contains(&mi)
                    || tp.cons.pairs.iter().any(|&(ti, x)| {
                        x == mi
                            && constraint_changed(
                                &a.constraints[ti],
                                &b.constraints[mi],
                                &tp.renames,
                            )
                    })
            };
            for (mi, c) in b.constraints.iter().enumerate() {
                if c.kind == ConstraintKind::Unique && new_or_changed_con(mi) {
                    online.insert((b.schema.clone(), b.name.clone(), sorted(&c.columns)));
                }
            }
            for (mi, ix) in b.indexes.iter().enumerate() {
                let new_or_changed =
                    tp.idx.m_only.contains(&mi)
                        || tp.idx.pairs.iter().any(|&(ti, x)| {
                            x == mi && index_changed(&a.indexes[ti], ix, &tp.renames)
                        });
                if !(ix.unique && ix.predicate.is_none() && new_or_changed) {
                    continue;
                }
                let attnums: Option<Vec<i16>> = ix
                    .keys
                    .iter()
                    .map(|k| {
                        b.columns
                            .iter()
                            .find(|c| ident(&c.name) == *k)
                            .map(|c| c.attnum)
                    })
                    .collect();
                if let Some(cols) = attnums {
                    online.insert((b.schema.clone(), b.name.clone(), sorted(&cols)));
                }
            }
        }
        self.online_uniques = online;
    }
}

// ---- roles (phase 0) ----

/// A new phase 0 step (ids `role:<verb>:<name>`).
fn role_out(verb: &str, name: &str, sql: String) -> Out {
    let mut o = Out::new(
        Bucket::Roles,
        Phase::Roles,
        StepKind::Generated,
        format!("role:{verb}:{name}"),
        sql,
    );
    o.key(pkey("role", &[name]));
    o
}

/// Phase 0 (U2 through the roles path): renames first (E29), then creates and updates, and
/// the target's role OIDs that get a new name (for the rest of the plan). A merged role is the
/// target's role when it carries the target's OID (renamed or not), else the target's role of
/// that name. The owner ([`OWNER_ROLE`]) is never planned (E30, T14 minor 6); roles are never
/// dropped (a role the merged model leaves out is reported). BYPASSRLS and owner-level, created
/// or turned on, are `security` (P9, E28); a rename is `breaking` (E29a).
fn role_steps(
    t: &SchemaModel,
    m: &SchemaModel,
) -> Result<(Vec<Out>, HashMap<Oid, String>), SchemaError> {
    // By the target's OID; else by name, a target role no merged role claims by OID.
    let paired = |mr: &Role| -> Option<&Role> {
        t.roles.iter().find(|r| same_oid(r.oid, mr.oid)).or_else(|| {
            t.roles.iter().find(|r| {
                r.name == mr.name && !m.roles.iter().any(|x| same_oid(r.oid, x.oid))
            })
        })
    };
    let mut renames: Vec<(Oid, String, String)> = Vec::new();
    for mr in m.roles.iter().filter(|r| r.name != OWNER_ROLE) {
        if let Some(tr) = paired(mr)
            && tr.name != mr.name
            && tr.name != OWNER_ROLE
        {
            renames.push((tr.oid, tr.name.clone(), mr.name.clone()));
        }
    }
    let mut out = Vec::new();
    // Each rename once its new name is free: no other target role holds it, or the one that
    // does was renamed away already.
    let mut current: Vec<String> = t.roles.iter().map(|r| r.name.clone()).collect();
    let mut pending = renames.clone();
    while !pending.is_empty() {
        let Some(k) = pending.iter().position(|(_, _, to)| !current.contains(to)) else {
            let (_, from, to) = &pending[0];
            let cycle = pending.iter().any(|(_, f, _)| f == to);
            return Err(SchemaError::Unplannable {
                object: format!("role {from}"),
                reason: if cycle {
                    "the source's role renames form a cycle; rename these roles on the target \
                     before merging"
                        .to_string()
                } else {
                    format!("the source renames {from} to {to}, a name another role on the target keeps")
                },
            });
        };
        let (oid, from, to) = pending.remove(k);
        current.retain(|n| *n != from);
        current.push(to.clone());
        let mut o = role_out(
            "rename",
            &from,
            format!("ALTER ROLE {} RENAME TO {}", ident(&from), ident(&to)),
        );
        o.key(pkey("role", &[&to]));
        o.breaking = Some(Breaking::RoleRename {
            role: from.clone(),
            new_name: to.clone(),
        });
        o.role_op = Some(RoleOp::Rename { oid, from, to });
        out.push(o);
    }
    // Creates and updates against the target as it reads after the renames (fix round M1):
    // memberships name the renamed roles by their new names.
    let renamed: HashMap<Oid, String> = renames
        .iter()
        .map(|(oid, _, to)| (*oid, to.clone()))
        .collect();
    let t = &t.rename_roles(|oid, _| renamed.get(&oid).cloned());
    let paired = |mr: &Role| -> Option<&Role> {
        t.roles.iter().find(|r| same_oid(r.oid, mr.oid)).or_else(|| {
            t.roles.iter().find(|r| {
                r.name == mr.name && !m.roles.iter().any(|x| same_oid(r.oid, x.oid))
            })
        })
    };
    for mr in m.roles.iter().filter(|r| r.name != OWNER_ROLE) {
        match paired(mr) {
            None => {
                let spec = RoleSpecModel {
                    name: mr.name.clone(),
                    owner_level: mr.owner_level(),
                    login: mr.login,
                    inherit: mr.inherit,
                    connection_limit: mr.connection_limit,
                    valid_until: mr.valid_until.as_deref().and_then(ddl::rfc3339),
                    bypass_rls: mr.bypass_rls,
                    member_of: memberships(mr),
                };
                let mut o = role_out("create", &mr.name, create_role_sql(&spec));
                o.security = spec.bypass_rls || spec.owner_level;
                o.role_op = Some(RoleOp::Create(spec));
                out.push(o);
            }
            Some(tr) if tr.name == OWNER_ROLE => {}
            Some(tr) => {
                let mut patch = RolePatchModel::default();
                if tr.owner_level() != mr.owner_level() {
                    patch.owner_level = Some(mr.owner_level());
                }
                if tr.login != mr.login {
                    patch.login = Some(mr.login);
                }
                if tr.inherit != mr.inherit {
                    patch.inherit = Some(mr.inherit);
                }
                if tr.connection_limit != mr.connection_limit {
                    patch.connection_limit = Some(mr.connection_limit);
                }
                if tr.valid_until != mr.valid_until {
                    patch.valid_until = Some(mr.valid_until.as_deref().and_then(ddl::rfc3339));
                }
                if tr.bypass_rls != mr.bypass_rls {
                    patch.bypass_rls = Some(mr.bypass_rls);
                }
                let (tm, mm) = (memberships(tr), memberships(mr));
                if tm != mm {
                    patch.member_of = Some(mm);
                }
                if patch == RolePatchModel::default() {
                    continue;
                }
                let sql = update_role_sql(&mr.name, &patch, &tm, tr.owner_level());
                let mut o = role_out("update", &mr.name, sql);
                o.security = patch.bypass_rls == Some(true) || patch.owner_level == Some(true);
                o.role_op = Some(RoleOp::Update {
                    name: mr.name.clone(),
                    patch,
                });
                out.push(o);
            }
        }
    }
    Ok((out, renamed))
}

fn create_role_sql(spec: &RoleSpecModel) -> String {
    let mut s = format!(
        "CREATE ROLE {} WITH {} {} {} CONNECTION LIMIT {}",
        ident(&spec.name),
        if spec.login { "LOGIN" } else { "NOLOGIN" },
        if spec.inherit { "INHERIT" } else { "NOINHERIT" },
        if spec.bypass_rls {
            "BYPASSRLS"
        } else {
            "NOBYPASSRLS"
        },
        spec.connection_limit
    );
    if let Some(v) = &spec.valid_until {
        s.push_str(&format!(" VALID UNTIL {}", literal(v)));
    }
    if spec.owner_level {
        s.push_str(&format!(
            ";\nGRANT {} TO {}",
            ident(OWNER_LEVEL_ROLE),
            ident(&spec.name)
        ));
    }
    for r in &spec.member_of {
        s.push_str(&format!(";\nGRANT {} TO {}", ident(r), ident(&spec.name)));
    }
    s
}

fn update_role_sql(
    name: &str,
    p: &RolePatchModel,
    current: &[String],
    owner_level: bool,
) -> String {
    let mut attrs = Vec::new();
    if let Some(l) = p.login {
        attrs.push(if l { "LOGIN" } else { "NOLOGIN" }.to_string());
    }
    if let Some(i) = p.inherit {
        attrs.push(if i { "INHERIT" } else { "NOINHERIT" }.to_string());
    }
    if let Some(b) = p.bypass_rls {
        attrs.push(if b { "BYPASSRLS" } else { "NOBYPASSRLS" }.to_string());
    }
    if let Some(c) = p.connection_limit {
        attrs.push(format!("CONNECTION LIMIT {c}"));
    }
    if let Some(v) = &p.valid_until {
        attrs.push(format!(
            "VALID UNTIL {}",
            v.as_deref().map_or_else(|| literal("infinity"), literal)
        ));
    }
    let mut stmts = Vec::new();
    if !attrs.is_empty() {
        stmts.push(format!(
            "ALTER ROLE {} WITH {}",
            ident(name),
            attrs.join(" ")
        ));
    }
    if let Some(o) = p.owner_level {
        let verb = if o { "GRANT" } else { "REVOKE" };
        let dir = if o { "TO" } else { "FROM" };
        if o != owner_level {
            stmts.push(format!(
                "{verb} {} {dir} {}",
                ident(OWNER_LEVEL_ROLE),
                ident(name)
            ));
        }
    }
    if let Some(ms) = &p.member_of {
        for r in ms.iter().filter(|r| !current.contains(r)) {
            stmts.push(format!("GRANT {} TO {}", ident(r), ident(name)));
        }
        for r in current.iter().filter(|r| !ms.contains(r)) {
            stmts.push(format!("REVOKE {} FROM {}", ident(r), ident(name)));
        }
    }
    stmts.join(";\n")
}
