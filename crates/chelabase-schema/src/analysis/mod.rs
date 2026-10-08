//! Three-way analysis, conflicts, integrity, resolutions and the merged model (T3).
//!
//! The types here are pinned for T4 (planner) and the API; T3 fills the functions and may add
//! fields or variants, but not rename or remove these.
//!
//! Layout: [`index`] keys every sub-object of one model and gives it a canonical form;
//! [`classify`] compares base, target and source per item and forms the conflicts;
//! [`integrity`] checks the references the merged model must resolve and lists the reported
//! objects merged items depend on; [`merge`] applies the resolutions and assembles the model.
//!
//! Rules beyond the spec's Part 3 table (rulings of T3, listed in its report):
//! - one item per sub-object: schema, table, column, constraint, index, policy, enum (with its
//!   values: Postgres can add and rename values, so a source change that drops or reorders a
//!   value the target has is an `Edit` conflict that take source can't resolve), sequence, role
//!   (with its memberships), default-privilege row, and one per grant;
//! - a schema or table dropped on one side while the other side changes it or anything in it is
//!   one `DropVsEdit` conflict over the whole subtree;
//! - integrity covers parents, a constraint's own columns, the columns an index or a policy or a
//!   generated expression names, owners, memberships and enum column types; a reference to an
//!   object outside the model (an excluded role or schema) always resolves;
//! - an object taken from the source whose definition text names something the merged model
//!   calls differently (the target renamed it) is a `Dependency` conflict; texts are never
//!   rewritten (E12a);
//! - the other way round, an object kept from the target whose text names something the merge
//!   renames is listed in [`MergedModel::follows_rename`] (E31): Postgres rewrites its text when
//!   the rename runs, so it's compared with what the rehearsal produced;
//! - a role is merged only in what the roles path makes (U2, E29: its name, attributes,
//!   owner-level, memberships by role); anything else, and every change to the owner, is
//!   reported "apply by hand" ([`roles`], E30) and the merged model keeps the target's;
//! - an index the source left INVALID is reported, never merged (T14 I4);
//! - an item whose missing references are all decided by one other conflict joins that conflict
//!   instead of getting a dependency conflict of its own;
//! - a resolution for a conflict that no longer exists is ignored; the last resolution for a
//!   conflict wins; a column order conflict takes only target-columns-first or own SQL (E12b).

mod classify;
mod index;
mod integrity;
mod merge;
mod roles;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

use crate::identity::{Direction, IdentityMap};
use crate::model::{
    Column, Constraint, DefaultPrivilege, EnumType, Grant, Index, Oid, Policy, Reported, Role,
    Schema, SchemaModel, Sequence, Table,
};
use index::{Ctx, SideIndex, Who};

/// The OID of an object in a [`MergedModel`] that the target doesn't have yet (the planner
/// creates it). Every other object in the merged model carries the target's OID (columns: the
/// target's `attnum`; new columns get the next `attnum`s after the target table's last one).
pub const NEW_OID: Oid = Oid(0);

/// A changed sub-object's stable id, derived from its key, e.g. `column:<base table oid>:<attnum>`
/// or `table:name:<schema id>/<name>` (e.g. `table:name:schema:2200/audit`) for an object newer
/// than the base (see `index` for every shape). Resolutions survive a recompute when the same
/// id appears again.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ItemId(pub String);

/// A conflict's stable id (derived like [`ItemId`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConflictId(pub String);

/// The kind of a merged sub-object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjKind {
    Schema,
    Table,
    Column,
    Constraint,
    Index,
    Policy,
    Grant,
    Enum,
    Sequence,
    Role,
    DefaultPrivilege,
}

/// A sub-object's readable key (as the target or source names it).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjKey {
    pub kind: ObjKind,
    pub schema: Option<String>,
    /// The table of a column, constraint, index, policy or table/column grant.
    pub table: Option<String>,
    pub name: String,
}

/// What a grant is on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantObject {
    Schema {
        name: String,
    },
    Table {
        schema: String,
        name: String,
    },
    Column {
        schema: String,
        table: String,
        column: String,
    },
    Sequence {
        schema: String,
        name: String,
    },
}

/// One sub-object as one side has it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjSnapshot {
    /// Always built with [`ObjSnapshot::schema`] ([`Schema::shallow`]): grants are items.
    Schema(Schema),
    /// The table's own properties only, always built with [`ObjSnapshot::table`] (that is,
    /// [`Table::shallow`]): its columns, constraints, indexes, policies and grants are items of
    /// their own, so the nested lists are empty. T3 must never wrap a full `Table` here.
    Table(Table),
    Column {
        table_oid: Oid,
        schema: String,
        table: String,
        column: Column,
    },
    Constraint {
        table_oid: Oid,
        schema: String,
        table: String,
        constraint: Constraint,
    },
    Index {
        table_oid: Oid,
        schema: String,
        table: String,
        index: Index,
    },
    Policy {
        table_oid: Oid,
        schema: String,
        table: String,
        policy: Policy,
    },
    Grant {
        on: GrantObject,
        grant: Grant,
    },
    /// The whole type, values included (one item per enum).
    Enum(EnumType),
    /// Always built with [`ObjSnapshot::sequence`] ([`Sequence::shallow`]): grants are items.
    Sequence(Sequence),
    Role(Role),
    DefaultPrivilege(DefaultPrivilege),
}

impl ObjSnapshot {
    /// A table's snapshot: its own properties only ([`Table::shallow`]).
    pub fn table(t: &Table) -> ObjSnapshot {
        ObjSnapshot::Table(t.shallow())
    }

    /// A schema's snapshot: its own properties only ([`Schema::shallow`]; grants are items).
    pub fn schema(s: &Schema) -> ObjSnapshot {
        ObjSnapshot::Schema(s.shallow())
    }

    /// A sequence's snapshot: its own properties only ([`Sequence::shallow`]; grants are items).
    pub fn sequence(s: &Sequence) -> ObjSnapshot {
        ObjSnapshot::Sequence(s.shallow())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Create(ObjSnapshot),
    Drop(ObjSnapshot),
    /// Properties changed, the name included (a rename or `SET SCHEMA` is an alter).
    /// Boxed so a `Change` stays the size of one snapshot (serializes as the plain snapshots).
    Alter {
        before: Box<ObjSnapshot>,
        after: Box<ObjSnapshot>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Changed on the source only: applied.
    Apply,
    /// Changed on the target only: nothing to do.
    TargetOnly,
    /// The same change on both sides: nothing to do.
    Overlap,
    Conflict(ConflictId),
}

/// Every changed sub-object on either side, with its outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub key: ObjKey,
    pub source: Option<Change>,
    pub target: Option<Change>,
    pub outcome: Outcome,
    pub data_loss: bool,
    /// A role created with or gaining BYPASSRLS (P9) or owner-level (E28).
    pub security: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictKind {
    Edit,
    DropVsEdit,
    CreateCollision,
    ColumnOrder,
    Dependency,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: ConflictId,
    pub kind: ConflictKind,
    pub items: Vec<ItemId>,
    pub base: Option<ObjSnapshot>,
    pub target: Option<ObjSnapshot>,
    pub source: Option<ObjSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    KeepTarget,
    TakeSource,
    OwnSql(String),
    /// Only for [`ConflictKind::ColumnOrder`]: the target's columns first, then the source's.
    TargetColumnsFirst,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub conflict: ConflictId,
    pub choice: Choice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedChangeKind {
    Created,
    Dropped,
    Altered,
    /// Unchanged on the source, listed because a merged item depends on it.
    Unchanged,
}

/// A source-side change to a reported object, or a merged item's dependency on one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReportedChange {
    pub object: Reported,
    pub change: ReportedChangeKind,
    /// Merged items whose expressions use it (a default or policy calling a function).
    pub needed_by: Vec<ItemId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    /// Every changed sub-object on either side, with its outcome.
    pub items: Vec<Item>,
    pub conflicts: Vec<Conflict>,
    /// Source-side changes to reported objects, and dependencies on them; then the indexes the
    /// source left INVALID ([`ReportedKind::InvalidIndex`], T14 I4) and the role changes the
    /// merge can't make ([`ReportedKind::Role`], E30, "apply by hand").
    ///
    /// [`ReportedKind::InvalidIndex`]: crate::model::ReportedKind::InvalidIndex
    /// [`ReportedKind::Role`]: crate::model::ReportedKind::Role
    pub reported: Vec<ReportedChange>,
}

/// `M = T + applied changes + resolutions`, with the own SQL that replaces resolved items.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MergedModel {
    pub model: SchemaModel,
    pub own_sql: Vec<(ConflictId, String)>,
    /// E31: objects M keeps from the target whose definition text names an object the merge
    /// renames (a target index on a column the source renamed). M holds the target's text, which
    /// Postgres rewrites when the rename runs, so these are checked against what the rehearsal
    /// produced ([`crate::compare::with_rehearsed`]), like own SQL's objects. Keys as M names
    /// them.
    #[serde(default)]
    pub follows_rename: Vec<FollowsRename>,
}

/// E31: an object of `M` (kept from the target) whose text names objects the merge renames.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowsRename {
    /// The object, as `M` names it.
    pub key: ObjKey,
    /// The renames its text follows (plain names: a column, or a table's, enum's or sequence's
    /// own name), which Postgres applies to the text when the rename runs.
    pub renames: Vec<NameChange>,
}

/// One rename: the target's name and `M`'s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameChange {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeError {
    #[error("{} conflict(s) have no resolution", .0.len())]
    Unresolved(Vec<ConflictId>),
    #[error("conflict {}: {reason}", conflict.0)]
    InvalidChoice {
        conflict: ConflictId,
        reason: String,
    },
    /// The resolutions leave a reference dangling (an FK, policy role, grantee, owned-by column,
    /// identity sequence, owner, membership or enum type naming an object the merged model
    /// doesn't have), e.g. take source on a dependency whose referenced item another conflict
    /// keeps out. Not raised when own SQL takes part: the rehearsal checks those.
    #[error("{} references {} item(s) the merged model doesn't have", item.0, missing.len())]
    BrokenReference { item: ItemId, missing: Vec<ItemId> },
}

/// The three models' indexes, with the direction's sides.
pub(crate) struct World<'a> {
    pub base: Ctx<'a>,
    pub target: Ctx<'a>,
    pub source: Ctx<'a>,
    pub bi: SideIndex,
    pub ti: SideIndex,
    pub si: SideIndex,
    pub dir: Direction,
}

impl<'a> World<'a> {
    fn new(
        base: &'a SchemaModel,
        target: &'a SchemaModel,
        source: &'a SchemaModel,
        map: &'a IdentityMap,
        dir: Direction,
    ) -> Self {
        let base = Ctx::new(base, Who::Base, map);
        let target = Ctx::new(target, Who::Side(dir.target_side()), map);
        let source = Ctx::new(source, Who::Side(dir.source_side()), map);
        let (bi, ti, si) = (
            index::build(&base),
            index::build(&target),
            index::build(&source),
        );
        World {
            base,
            target,
            source,
            bi,
            ti,
            si,
            dir,
        }
    }

    /// The containing object's id, as whichever model has the object names it.
    pub fn parent_of(&self, id: &ItemId) -> Option<&ItemId> {
        self.si
            .get(id)
            .or_else(|| self.ti.get(id))
            .or_else(|| self.bi.get(id))
            .and_then(|e| e.parent.as_ref())
    }

    /// The object as the base, the target and the source have it.
    pub fn snaps(
        &self,
        id: &ItemId,
    ) -> (
        Option<ObjSnapshot>,
        Option<ObjSnapshot>,
        Option<ObjSnapshot>,
    ) {
        let snap = |i: &SideIndex| i.get(id).map(|e| e.snap.clone());
        (snap(&self.bi), snap(&self.ti), snap(&self.si))
    }
}

/// The three-way analysis: `diff(B,S)` and `diff(B,T)` per sub-object, outcomes, conflicts and
/// the merged model's integrity check. Items and conflicts are sorted by id.
///
/// **Re-baseline (E12c):** pass an empty [`IdentityMap`]. Nothing then has a base identity,
/// every object of all three models is matched by qualified name, and a rename reads as a drop
/// plus a create (with data loss on the drop).
pub fn analyze(
    base: &SchemaModel,
    target: &SchemaModel,
    source: &SchemaModel,
    map: &IdentityMap,
    dir: Direction,
) -> Analysis {
    let w = World::new(base, target, source, map, dir);
    let mut d = classify::classify(&w);
    integrity::add_dependency_conflicts(&w, &mut d);
    classify::flags(&w, &mut d);
    let mut reported = integrity::reported(&w, &d);
    reported.extend(integrity::invalid_indexes(&w));
    reported.extend(roles::reports(&w));
    Analysis {
        items: d.items.into_values().collect(),
        conflicts: d.conflicts.into_values().collect(),
        reported,
    }
}

/// The merged model, given a resolution for every conflict of `analysis` (computed from the
/// same models, map and direction).
pub fn merge(
    analysis: &Analysis,
    resolutions: &[Resolution],
    base: &SchemaModel,
    target: &SchemaModel,
    source: &SchemaModel,
    map: &IdentityMap,
    dir: Direction,
) -> Result<MergedModel, MergeError> {
    let w = World::new(base, target, source, map, dir);
    merge::merge(analysis, resolutions, &w)
}
