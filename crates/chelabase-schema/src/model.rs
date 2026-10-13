//! The normalized schema model: one Postgres database's merged objects (Part 1 of the spec),
//! each with its catalog OID, plus the objects the merge only reports.
//!
//! Text fields hold Postgres's own normalized forms (`format_type`, `pg_get_expr`,
//! `pg_get_constraintdef(oid, true)`, …), read with `search_path = pg_catalog`, so they compare
//! stably. The JSON field names are the contract for the catalog reader and stored snapshots.
//! [`SchemaModel::normalize`] sorts every list whose order carries no meaning, so serialization is
//! deterministic.

use serde::{Deserialize, Serialize};

/// A catalog OID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Oid(pub u32);

/// Catalog class of an object; with an [`Oid`] it is the object's identity at the base.
/// Columns are identified by their table's OID plus `attnum`; enum values by their own
/// `pg_enum` OID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjClass {
    Schema,
    Table,
    Column,
    Constraint,
    Index,
    Enum,
    /// One enum value ([`EnumLabel`]), keyed by its `pg_enum` OID (no sub).
    EnumLabel,
    Sequence,
    Policy,
    Role,
}

/// One database's schema.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SchemaModel {
    pub model_version: u32,
    pub server_major: u32,
    pub schemas: Vec<Schema>,
    /// Ordinary tables (`relkind = 'r'`, not partitions) with their columns, constraints,
    /// indexes, policies and grants nested.
    pub tables: Vec<Table>,
    pub enums: Vec<EnumType>,
    /// Standalone sequences and sequences OWNED BY a column; identity sequences live on their
    /// column ([`Column::identity`]).
    pub sequences: Vec<Sequence>,
    pub roles: Vec<Role>,
    pub default_privileges: Vec<DefaultPrivilege>,
    /// Objects the merge doesn't move, listed so the preview can show source-side changes.
    pub reported: Vec<Reported>,
}

/// A role named by OID and name (owners, grantors, memberships).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct NamedRole {
    #[cfg_attr(feature = "openapi", schema(value_type = u32))]
    pub oid: Oid,
    pub name: String,
}

/// A grantee or policy role: `PUBLIC` (grantee 0, `polroles = {0}`) or a role.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum RoleRef {
    Public,
    Role {
        #[cfg_attr(feature = "openapi", schema(value_type = u32))]
        oid: Oid,
        name: String,
    },
}

/// One privilege from an ACL (`aclexplode`). Column privileges live on the column
/// ([`Column::grants`]), so a column's identity (table OID + attnum) carries them through renames.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Grant {
    pub grantor: NamedRole,
    pub grantee: RoleRef,
    /// Upper case, as `aclexplode` gives it: `SELECT`, `INSERT`, `USAGE`, …
    pub privilege: String,
    pub grantable: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub oid: Oid,
    pub name: String,
    pub owner: NamedRole,
    pub comment: Option<String>,
    pub grants: Vec<Grant>,
}

impl Schema {
    /// The schema's own properties only: its grants emptied (each grant is an item of its own
    /// in the analysis). A schema snapshot (`analysis::ObjSnapshot::Schema`) is always taken
    /// with this.
    pub fn shallow(&self) -> Schema {
        Schema {
            grants: Vec::new(),
            ..self.clone()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Table {
    #[cfg_attr(feature = "openapi", schema(value_type = u32))]
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    pub owner: NamedRole,
    pub comment: Option<String>,
    pub rls_enabled: bool,
    pub rls_forced: bool,
    /// `relpersistence = 'u'`.
    pub unlogged: bool,
    /// `reloptions` (`fillfactor=70`, …), sorted.
    pub options: Vec<String>,
    /// In `attnum` order (dropped columns excluded).
    pub columns: Vec<Column>,
    /// Every `pg_constraint` row of the table (PG17 has no NOT NULL constraints there).
    pub constraints: Vec<Constraint>,
    /// Indexes that don't back a constraint (`conindid`).
    pub indexes: Vec<Index>,
    pub policies: Vec<Policy>,
    /// Table-level privileges (`relacl`, or `acldefault('r', relowner)` when NULL).
    pub grants: Vec<Grant>,
}

impl Table {
    /// The table's own properties only: columns, constraints, indexes, policies and grants
    /// emptied (each of those is an item of its own in the analysis). A table snapshot
    /// (`analysis::ObjSnapshot::Table`) is always taken with this.
    pub fn shallow(&self) -> Table {
        Table {
            oid: self.oid,
            schema: self.schema.clone(),
            name: self.name.clone(),
            owner: self.owner.clone(),
            comment: self.comment.clone(),
            rls_enabled: self.rls_enabled,
            rls_forced: self.rls_forced,
            unlogged: self.unlogged,
            options: self.options.clone(),
            columns: Vec::new(),
            constraints: Vec::new(),
            indexes: Vec::new(),
            policies: Vec::new(),
            grants: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Column {
    pub attnum: i16,
    pub name: String,
    /// `format_type(atttypid, atttypmod)`.
    pub data_type: String,
    /// `atttypid`: lets the analysis follow a renamed user type (an enum) that existed at the base.
    #[cfg_attr(feature = "openapi", schema(value_type = u32))]
    pub type_oid: Oid,
    /// Qualified collation name, only when it isn't the type's default.
    pub collation: Option<String>,
    /// `pg_get_expr(adbin, adrelid)`; `None` for a generated column (see `generated`).
    pub default: Option<String>,
    /// PG17's `attnotnull`.
    pub not_null: bool,
    pub identity: Option<Identity>,
    /// The `GENERATED ALWAYS AS (…) STORED` expression.
    pub generated: Option<String>,
    pub comment: Option<String>,
    /// Column privileges (`attacl`, only when non-NULL).
    pub grants: Vec<Grant>,
}

impl Column {
    /// The column's own properties only: its grants and its identity sequence's grants emptied
    /// (each grant is an item of its own in the analysis).
    pub fn shallow(&self) -> Column {
        let mut c = self.clone();
        c.grants = Vec::new();
        if let Some(identity) = &mut c.identity {
            identity.grants = Vec::new();
        }
        c
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum IdentityGeneration {
    /// `attidentity = 'a'`.
    Always,
    /// `attidentity = 'd'`.
    ByDefault,
}

/// An identity column's generation and its sequence (`pg_depend` type `i`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Identity {
    pub generation: IdentityGeneration,
    #[cfg_attr(feature = "openapi", schema(value_type = u32))]
    pub sequence_oid: Oid,
    /// The sequence's name (in the table's schema).
    pub sequence_name: String,
    pub params: SequenceParams,
    /// Privileges on the identity sequence (its `relacl`, or the default when NULL).
    pub grants: Vec<Grant>,
}

/// A sequence's parameters (`pg_sequence`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct SequenceParams {
    /// `format_type(seqtypid, NULL)`: `smallint`, `integer` or `bigint`.
    pub data_type: String,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    /// `p`
    PrimaryKey,
    /// `u`
    Unique,
    /// `f`
    ForeignKey,
    /// `c`
    Check,
    /// `x`
    Exclusion,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Constraint {
    #[cfg_attr(feature = "openapi", schema(value_type = u32))]
    pub oid: Oid,
    pub name: String,
    pub kind: ConstraintKind,
    /// `pg_get_constraintdef(oid, true)`.
    pub definition: String,
    /// `conkey`, in order (empty for a table-level check without columns).
    pub columns: Vec<i16>,
    /// A foreign key's referenced table and columns.
    pub references: Option<ForeignKeyTarget>,
    pub deferrable: bool,
    pub initially_deferred: bool,
    /// `convalidated`: `false` for a `NOT VALID` constraint.
    pub validated: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct ForeignKeyTarget {
    #[cfg_attr(feature = "openapi", schema(value_type = u32))]
    pub table_oid: Oid,
    pub schema: String,
    pub table: String,
    /// `confkey`, in order.
    pub columns: Vec<i16>,
}

/// An index, in normalized parts so its name isn't part of its definition (a rename is only a
/// name change).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema), schema(as = TableIndex))]
pub struct Index {
    #[cfg_attr(feature = "openapi", schema(value_type = u32))]
    pub oid: Oid,
    pub name: String,
    /// Access method: `btree`, `gin`, …
    pub method: String,
    pub unique: bool,
    pub nulls_not_distinct: bool,
    /// Key columns or expressions, in order, each as in `CREATE INDEX` (with collation,
    /// operator class and ordering when not the default).
    pub keys: Vec<String>,
    /// `INCLUDE` columns, in order.
    pub include: Vec<String>,
    /// The `WHERE` predicate.
    pub predicate: Option<String>,
    /// Storage parameters (`fillfactor=70`, …), sorted.
    pub options: Vec<String>,
    /// `indisvalid`: an interrupted `CREATE INDEX CONCURRENTLY` leaves `false`.
    pub valid: bool,
    pub comment: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum PolicyCommand {
    /// `*`
    All,
    /// `r`
    Select,
    /// `a`
    Insert,
    /// `w`
    Update,
    /// `d`
    Delete,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct Policy {
    #[cfg_attr(feature = "openapi", schema(value_type = u32))]
    pub oid: Oid,
    pub name: String,
    pub command: PolicyCommand,
    /// `false` for `AS RESTRICTIVE`.
    pub permissive: bool,
    /// Sorted; `[Public]` for `polroles = {0}`.
    pub roles: Vec<RoleRef>,
    pub using: Option<String>,
    pub with_check: Option<String>,
    /// T14 minor 14. Absent in a snapshot stored before it was read: `None`.
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnumType {
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    pub owner: NamedRole,
    pub comment: Option<String>,
    /// In `enumsortorder` order (never sorted by the model; the raw sort value isn't kept).
    pub labels: Vec<EnumLabel>,
}

/// One enum value. Its own `pg_enum` OID survives `RENAME VALUE`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnumLabel {
    pub oid: Oid,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    pub oid: Oid,
    pub schema: String,
    pub name: String,
    pub owner: NamedRole,
    pub comment: Option<String>,
    pub params: SequenceParams,
    /// `OWNED BY` a column (`pg_depend` type `a`).
    pub owned_by: Option<OwnedBy>,
    pub grants: Vec<Grant>,
}

impl Sequence {
    /// The sequence's own properties only: its grants emptied (each grant is an item of its
    /// own in the analysis). A sequence snapshot (`analysis::ObjSnapshot::Sequence`) is always
    /// taken with this.
    pub fn shallow(&self) -> Sequence {
        Sequence {
            grants: Vec::new(),
            ..self.clone()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnedBy {
    pub table_oid: Oid,
    pub schema: String,
    pub table: String,
    pub attnum: i16,
    pub column: String,
}

/// A role (from `pg_roles`; passwords are never read). The merged attributes are the U2 set;
/// owner-level is a membership in `neon_superuser` ([`Role::owner_level`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Role {
    pub oid: Oid,
    pub name: String,
    pub login: bool,
    pub inherit: bool,
    /// `-1` for no limit.
    pub connection_limit: i32,
    /// `rolvaliduntil` as Postgres prints a `timestamptz` (`None` for no expiry).
    pub valid_until: Option<String>,
    pub bypass_rls: bool,
    /// Memberships (`pg_auth_members`), sorted by role name.
    pub member_of: Vec<Membership>,
    /// Attributes outside the U2 set that are on (`SUPERUSER`, `CREATEDB`, `CREATEROLE`,
    /// `REPLICATION`), sorted; reported, never merged.
    pub other_attributes: Vec<String>,
    pub comment: Option<String>,
}

/// The role that grants owner-level access.
pub const OWNER_LEVEL_ROLE: &str = "neon_superuser";

/// Whether the merge makes a membership in `role` (U2, through the roles path): owner-level
/// (`neon_superuser`), or any role the roles path lets a role join. A membership in the owner,
/// a reserved role, a `chelabase_…`, a `cb_m_…` or a `pg_…` role is refused there
/// (`is_membership_reserved`), so it's reported, never merged (E30).
pub fn membership_merges(role: &str) -> bool {
    role == OWNER_LEVEL_ROLE || !chelabase_types::role_name::is_membership_reserved(role)
}

impl Role {
    /// Owner-level: a member of [`OWNER_LEVEL_ROLE`].
    pub fn owner_level(&self) -> bool {
        self.member_of
            .iter()
            .any(|m| m.role.name == OWNER_LEVEL_ROLE)
    }
}

/// One `pg_auth_members` row. Since PG16 a role can be granted to the same member several
/// times by different grantors, each row with its own options.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Membership {
    /// The role this role is a member of.
    pub role: NamedRole,
    pub grantor: NamedRole,
    pub admin: bool,
    pub inherit: bool,
    pub set: bool,
}

/// The bootstrap superuser (OID 10; `cloud_admin` on a Neon compute, which runs `initdb` as
/// it): the grantor of the ADMIN membership Postgres gives a role's creator.
pub const BOOTSTRAP_SUPERUSER: (Oid, &str) = (Oid(10), "cloud_admin");

/// E22: the memberships Postgres 16+ gives `owner` in `role` when `owner` (not a superuser,
/// with CREATEROLE) creates it, as the merge's phase 0 does (`SET ROLE` to the owner, or the
/// owner's own session): an ADMIN grant from the bootstrap superuser
/// ([`BOOTSTRAP_SUPERUSER`]), and the `createrole_self_grant = 'set, inherit'` grant (the
/// compute's setting) from the owner itself. Checked live by `merge_live`
/// `owner_memberships_of_created_roles`.
pub fn owner_grants(role: &NamedRole, owner: &NamedRole) -> Vec<Membership> {
    vec![
        Membership {
            role: role.clone(),
            grantor: NamedRole {
                oid: BOOTSTRAP_SUPERUSER.0,
                name: BOOTSTRAP_SUPERUSER.1.to_string(),
            },
            admin: true,
            inherit: false,
            set: false,
        },
        Membership {
            role: role.clone(),
            grantor: owner.clone(),
            admin: false,
            inherit: true,
            set: true,
        },
    ]
}

/// `pg_default_acl.defaclobjtype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultPrivilegeObject {
    /// `r`
    Tables,
    /// `S`
    Sequences,
    /// `f`
    Functions,
    /// `T`
    Types,
    /// `n`
    Schemas,
}

/// One `pg_default_acl` row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DefaultPrivilege {
    pub oid: Oid,
    pub role: NamedRole,
    /// `None` for a global row.
    pub schema: Option<String>,
    pub object_type: DefaultPrivilegeObject,
    pub grants: Vec<Grant>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedKind {
    View,
    MaterializedView,
    Function,
    Procedure,
    Trigger,
    PartitionedTable,
    Partition,
    /// A table that `INHERITS` from another (not a partition): not merged, since a merge would
    /// re-create it without its parent link (T14 M6). `parent` is its first parent.
    InheritedTable,
    ForeignTable,
    Domain,
    CompositeType,
    Extension,
    Rule,
    Cast,
    Operator,
    Publication,
    EventTrigger,
    /// A `chelabase_merge_nn_…` check or `chelabase_merge_ix_…` index left by an interrupted merge.
    MergeLeftover,
    /// An index the source left INVALID (an interrupted `CREATE INDEX CONCURRENTLY`): never
    /// merged (T14 I4). `parent` is its table, `definition` its `CREATE INDEX`.
    InvalidIndex,
    /// A role change the merge can't make through the roles path (E30): other attributes
    /// (`CREATEDB`, `CREATEROLE`, `REPLICATION`), the comment, a membership the roles path refuses
    /// (`pg_*` or reserved roles), a membership's options, or any change to the owner itself.
    /// `name` is the role as the merged model names it; `definition` is the SQL to apply by hand.
    Role,
    Other(String),
}

/// An object the merge doesn't move (6g), listed so the preview can show source-side changes and
/// the analysis can find merged objects that depend on it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reported {
    pub kind: ReportedKind,
    pub oid: Oid,
    /// `None` for objects outside a schema (casts, publications, event triggers).
    pub schema: Option<String>,
    pub name: String,
    /// The table a trigger, rule, partition or merge leftover belongs to.
    pub parent: Option<String>,
    /// A function's or procedure's identity arguments (`pg_get_function_identity_arguments`).
    pub arguments: Option<String>,
    /// Its definition text where Postgres has one (`pg_get_viewdef`, `pg_get_functiondef`, …).
    pub definition: Option<String>,
}

impl SchemaModel {
    /// An empty model of the current [`crate::MODEL_VERSION`].
    pub fn empty(server_major: u32) -> Self {
        SchemaModel {
            model_version: crate::MODEL_VERSION,
            server_major,
            schemas: Vec::new(),
            tables: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            roles: Vec::new(),
            default_privileges: Vec::new(),
            reported: Vec::new(),
        }
    }

    /// The model with roles renamed: every role `rename(oid, name)` gives a new name for (the
    /// role itself, and every reference to it: owners, grantors, grantees, policy roles,
    /// memberships, default-privilege roles), as Postgres shows them after `ALTER ROLE … RENAME`
    /// (E29). References keep their OIDs.
    pub fn rename_roles(&self, rename: impl Fn(Oid, &str) -> Option<String>) -> SchemaModel {
        let named = |r: &mut NamedRole| {
            if let Some(n) = rename(r.oid, &r.name) {
                r.name = n;
            }
        };
        let role_ref = |r: &mut RoleRef| {
            if let RoleRef::Role { oid, name } = r
                && let Some(n) = rename(*oid, name)
            {
                *name = n;
            }
        };
        let grants = |gs: &mut Vec<Grant>| {
            for g in gs {
                named(&mut g.grantor);
                role_ref(&mut g.grantee);
            }
        };
        let mut m = self.clone();
        for s in &mut m.schemas {
            named(&mut s.owner);
            grants(&mut s.grants);
        }
        for t in &mut m.tables {
            named(&mut t.owner);
            grants(&mut t.grants);
            for c in &mut t.columns {
                grants(&mut c.grants);
                if let Some(id) = &mut c.identity {
                    grants(&mut id.grants);
                }
            }
            for p in &mut t.policies {
                p.roles.iter_mut().for_each(role_ref);
            }
        }
        for e in &mut m.enums {
            named(&mut e.owner);
        }
        for q in &mut m.sequences {
            named(&mut q.owner);
            grants(&mut q.grants);
        }
        for r in &mut m.roles {
            if let Some(n) = rename(r.oid, &r.name) {
                r.name = n;
            }
            for ms in &mut r.member_of {
                named(&mut ms.role);
                named(&mut ms.grantor);
            }
        }
        for d in &mut m.default_privileges {
            named(&mut d.role);
            grants(&mut d.grants);
        }
        m
    }

    /// Sorts every list whose order carries no meaning: objects by `(schema, name)`, columns by
    /// `attnum`, grants, policy roles, memberships and option lists by their text. Ordered lists
    /// (enum labels, index keys and INCLUDE columns, constraint columns) are left as they are.
    pub fn normalize(&mut self) {
        for s in &mut self.schemas {
            sort_grants(&mut s.grants);
        }
        self.schemas
            .sort_by(|a, b| (&a.name, a.oid).cmp(&(&b.name, b.oid)));

        for t in &mut self.tables {
            t.options.sort();
            sort_grants(&mut t.grants);
            for c in &mut t.columns {
                sort_grants(&mut c.grants);
                if let Some(identity) = &mut c.identity {
                    sort_grants(&mut identity.grants);
                }
            }
            t.columns.sort_by_key(|c| c.attnum);
            t.constraints
                .sort_by(|a, b| (&a.name, a.oid).cmp(&(&b.name, b.oid)));
            for i in &mut t.indexes {
                i.options.sort();
            }
            t.indexes
                .sort_by(|a, b| (&a.name, a.oid).cmp(&(&b.name, b.oid)));
            for p in &mut t.policies {
                p.roles
                    .sort_by(|a, b| role_ref_key(a).cmp(&role_ref_key(b)));
            }
            t.policies
                .sort_by(|a, b| (&a.name, a.oid).cmp(&(&b.name, b.oid)));
        }
        self.tables
            .sort_by(|a, b| (&a.schema, &a.name, a.oid).cmp(&(&b.schema, &b.name, b.oid)));

        self.enums
            .sort_by(|a, b| (&a.schema, &a.name, a.oid).cmp(&(&b.schema, &b.name, b.oid)));

        for s in &mut self.sequences {
            sort_grants(&mut s.grants);
        }
        self.sequences
            .sort_by(|a, b| (&a.schema, &a.name, a.oid).cmp(&(&b.schema, &b.name, b.oid)));

        for r in &mut self.roles {
            r.member_of
                .sort_by(|a, b| membership_key(a).cmp(&membership_key(b)));
            r.other_attributes.sort();
        }
        self.roles
            .sort_by(|a, b| (&a.name, a.oid).cmp(&(&b.name, b.oid)));

        for d in &mut self.default_privileges {
            sort_grants(&mut d.grants);
        }
        self.default_privileges.sort_by(|a, b| {
            (&a.role.name, &a.schema, a.object_type, a.oid).cmp(&(
                &b.role.name,
                &b.schema,
                b.object_type,
                b.oid,
            ))
        });

        self.reported.sort_by(|a, b| {
            (&a.schema, &a.name, &a.parent, &a.arguments, &a.kind, a.oid).cmp(&(
                &b.schema,
                &b.name,
                &b.parent,
                &b.arguments,
                &b.kind,
                b.oid,
            ))
        });
    }
}

/// By role, then grantor (names, then OIDs), then the option flags.
#[allow(clippy::type_complexity)]
fn membership_key(m: &Membership) -> (&str, Oid, &str, Oid, bool, bool, bool) {
    (
        &m.role.name,
        m.role.oid,
        &m.grantor.name,
        m.grantor.oid,
        m.admin,
        m.inherit,
        m.set,
    )
}

/// `PUBLIC` first, then roles by name.
fn role_ref_key(r: &RoleRef) -> (u8, &str, Oid) {
    match r {
        RoleRef::Public => (0, "", Oid(0)),
        RoleRef::Role { oid, name } => (1, name, *oid),
    }
}

fn sort_grants(grants: &mut [Grant]) {
    grants.sort_by(|a, b| {
        (
            role_ref_key(&a.grantee),
            &a.privilege,
            a.grantable,
            &a.grantor,
        )
            .cmp(&(
                role_ref_key(&b.grantee),
                &b.privilege,
                b.grantable,
                &b.grantor,
            ))
    });
}

/// Serialization fixture only: it mixes facts no real catalog has (a PK with `references`, an
/// identity column with a default and a generation expression, …); never use it as a semantic
/// fixture for the analysis or the planner.
#[cfg(test)]
pub(crate) mod sample {
    use super::*;

    fn role(oid: u32, name: &str) -> NamedRole {
        NamedRole {
            oid: Oid(oid),
            name: name.to_string(),
        }
    }

    fn grant(grantee: RoleRef, privilege: &str) -> Grant {
        Grant {
            grantor: role(10, "postgres"),
            grantee,
            privilege: privilege.to_string(),
            grantable: true,
        }
    }

    fn params() -> SequenceParams {
        SequenceParams {
            data_type: "bigint".to_string(),
            start: 1,
            increment: 1,
            min: 1,
            max: i64::MAX,
            cache: 1,
            cycle: false,
        }
    }

    /// A model with every type and every optional field set, its unordered lists in reverse
    /// order (so `normalize` has work to do).
    pub(crate) fn full_model() -> SchemaModel {
        let app = RoleRef::Role {
            oid: Oid(16400),
            name: "app".to_string(),
        };
        let mut m = SchemaModel::empty(17);
        m.schemas = vec![
            Schema {
                oid: Oid(2200),
                name: "public".to_string(),
                owner: role(10, "postgres"),
                comment: Some("standard public schema".to_string()),
                grants: vec![
                    grant(RoleRef::Public, "USAGE"),
                    grant(app.clone(), "CREATE"),
                ],
            },
            Schema {
                oid: Oid(16390),
                name: "Billing".to_string(),
                owner: role(10, "postgres"),
                comment: Some("money".to_string()),
                grants: vec![],
            },
        ];
        m.tables = vec![
            Table {
                oid: Oid(16420),
                schema: "public".to_string(),
                name: "orders".to_string(),
                owner: role(10, "postgres"),
                comment: Some("orders".to_string()),
                rls_enabled: true,
                rls_forced: true,
                unlogged: true,
                options: vec![
                    "fillfactor=70".to_string(),
                    "autovacuum_enabled=false".to_string(),
                ],
                columns: vec![
                    Column {
                        attnum: 2,
                        name: "status".to_string(),
                        data_type: "\"Billing\".status".to_string(),
                        type_oid: Oid(16391),
                        collation: Some("pg_catalog.\"C\"".to_string()),
                        default: Some("'new'::\"Billing\".status".to_string()),
                        not_null: true,
                        identity: None,
                        generated: Some("(id * 2)".to_string()),
                        comment: Some("state".to_string()),
                        grants: vec![grant(app.clone(), "UPDATE"), grant(app.clone(), "SELECT")],
                    },
                    Column {
                        attnum: 1,
                        name: "id".to_string(),
                        data_type: "bigint".to_string(),
                        type_oid: Oid(20),
                        collation: Some("pg_catalog.default".to_string()),
                        default: Some("0".to_string()),
                        not_null: true,
                        identity: Some(Identity {
                            generation: IdentityGeneration::Always,
                            sequence_oid: Oid(16419),
                            sequence_name: "orders_id_seq".to_string(),
                            params: params(),
                            grants: vec![
                                grant(app.clone(), "USAGE"),
                                grant(RoleRef::Public, "SELECT"),
                            ],
                        }),
                        generated: Some("1".to_string()),
                        comment: Some("key".to_string()),
                        grants: vec![],
                    },
                ],
                constraints: vec![
                    Constraint {
                        oid: Oid(16430),
                        name: "orders_pkey".to_string(),
                        kind: ConstraintKind::PrimaryKey,
                        definition: "PRIMARY KEY (id)".to_string(),
                        columns: vec![1],
                        references: Some(ForeignKeyTarget {
                            table_oid: Oid(16420),
                            schema: "public".to_string(),
                            table: "orders".to_string(),
                            columns: vec![1],
                        }),
                        deferrable: true,
                        initially_deferred: true,
                        validated: true,
                    },
                    Constraint {
                        oid: Oid(16429),
                        name: "Order check".to_string(),
                        kind: ConstraintKind::Check,
                        definition: "CHECK (id > 0) NOT VALID".to_string(),
                        columns: vec![1],
                        references: None,
                        deferrable: false,
                        initially_deferred: false,
                        validated: false,
                    },
                ],
                indexes: vec![
                    Index {
                        oid: Oid(16440),
                        name: "orders_status_idx".to_string(),
                        method: "btree".to_string(),
                        unique: true,
                        nulls_not_distinct: true,
                        keys: vec!["status DESC".to_string(), "lower(id::text)".to_string()],
                        include: vec!["id".to_string()],
                        predicate: Some("(status <> 'gone'::\"Billing\".status)".to_string()),
                        options: vec![
                            "fillfactor=80".to_string(),
                            "deduplicate_items=off".to_string(),
                        ],
                        valid: true,
                        comment: Some("by status".to_string()),
                    },
                    Index {
                        oid: Oid(16439),
                        name: "a_idx".to_string(),
                        method: "hash".to_string(),
                        unique: false,
                        nulls_not_distinct: false,
                        keys: vec!["id".to_string()],
                        include: vec![],
                        predicate: None,
                        options: vec![],
                        valid: false,
                        comment: None,
                    },
                ],
                policies: vec![Policy {
                    oid: Oid(16450),
                    name: "own rows".to_string(),
                    command: PolicyCommand::Update,
                    permissive: false,
                    roles: vec![app.clone(), RoleRef::Public],
                    using: Some("(id = 1)".to_string()),
                    with_check: Some("(id = 2)".to_string()),
                    comment: Some("only mine".to_string()),
                }],
                grants: vec![
                    grant(app.clone(), "SELECT"),
                    grant(RoleRef::Public, "INSERT"),
                ],
            },
            Table {
                oid: Oid(16410),
                schema: "Billing".to_string(),
                name: "my table".to_string(),
                owner: role(16400, "app"),
                comment: None,
                rls_enabled: false,
                rls_forced: false,
                unlogged: false,
                options: vec![],
                columns: vec![],
                constraints: vec![],
                indexes: vec![],
                policies: vec![],
                grants: vec![],
            },
        ];
        m.enums = vec![EnumType {
            oid: Oid(16391),
            schema: "Billing".to_string(),
            name: "status".to_string(),
            owner: role(10, "postgres"),
            comment: Some("states".to_string()),
            labels: vec![
                EnumLabel {
                    oid: Oid(16393),
                    label: "new".to_string(),
                },
                EnumLabel {
                    oid: Oid(16392),
                    label: "gone".to_string(),
                },
            ],
        }];
        m.sequences = vec![
            Sequence {
                oid: Oid(16460),
                schema: "public".to_string(),
                name: "tickets".to_string(),
                owner: role(10, "postgres"),
                comment: Some("tickets".to_string()),
                params: SequenceParams {
                    cycle: true,
                    ..params()
                },
                owned_by: Some(OwnedBy {
                    table_oid: Oid(16420),
                    schema: "public".to_string(),
                    table: "orders".to_string(),
                    attnum: 2,
                    column: "status".to_string(),
                }),
                grants: vec![grant(app.clone(), "USAGE"), grant(app.clone(), "SELECT")],
            },
            Sequence {
                oid: Oid(16459),
                schema: "Billing".to_string(),
                name: "counter".to_string(),
                owner: role(10, "postgres"),
                comment: None,
                params: params(),
                owned_by: None,
                grants: vec![],
            },
        ];
        m.roles = vec![
            Role {
                oid: Oid(16400),
                name: "app".to_string(),
                login: true,
                inherit: true,
                connection_limit: 3,
                valid_until: Some("2030-01-01 00:00:00+00".to_string()),
                bypass_rls: true,
                member_of: vec![
                    // The same role granted twice, by different grantors (PG16+).
                    Membership {
                        role: role(16401, "readers"),
                        grantor: role(16402, "zed"),
                        admin: false,
                        inherit: false,
                        set: false,
                    },
                    Membership {
                        role: role(16401, "readers"),
                        grantor: role(10, "postgres"),
                        admin: true,
                        inherit: true,
                        set: true,
                    },
                    Membership {
                        role: role(16399, "neon_superuser"),
                        grantor: role(10, "postgres"),
                        admin: false,
                        inherit: true,
                        set: true,
                    },
                ],
                other_attributes: vec!["CREATEROLE".to_string(), "CREATEDB".to_string()],
                comment: Some("the app".to_string()),
            },
            Role {
                oid: Oid(16401),
                name: "Readers".to_string(),
                login: false,
                inherit: false,
                connection_limit: -1,
                valid_until: None,
                bypass_rls: false,
                member_of: vec![],
                other_attributes: vec![],
                comment: None,
            },
        ];
        m.default_privileges = vec![
            DefaultPrivilege {
                oid: Oid(16470),
                role: role(10, "postgres"),
                schema: Some("public".to_string()),
                object_type: DefaultPrivilegeObject::Tables,
                grants: vec![grant(app.clone(), "UPDATE"), grant(app.clone(), "SELECT")],
            },
            DefaultPrivilege {
                oid: Oid(16471),
                role: role(10, "postgres"),
                schema: None,
                object_type: DefaultPrivilegeObject::Sequences,
                grants: vec![grant(app.clone(), "USAGE")],
            },
        ];
        m.reported = vec![
            Reported {
                kind: ReportedKind::Function,
                oid: Oid(16480),
                schema: Some("public".to_string()),
                name: "touch".to_string(),
                parent: Some("orders".to_string()),
                arguments: Some("a integer".to_string()),
                definition: Some("CREATE FUNCTION public.touch(a integer) …".to_string()),
            },
            Reported {
                kind: ReportedKind::Other("aggregate".to_string()),
                oid: Oid(16481),
                schema: Some("Billing".to_string()),
                name: "agg".to_string(),
                parent: Some("x".to_string()),
                arguments: Some("integer".to_string()),
                definition: Some("…".to_string()),
            },
        ];
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reversed(mut m: SchemaModel) -> SchemaModel {
        m.schemas.reverse();
        m.tables.reverse();
        for t in &mut m.tables {
            t.options.reverse();
            t.columns.reverse();
            t.constraints.reverse();
            t.indexes.reverse();
            t.policies.reverse();
            t.grants.reverse();
            for c in &mut t.columns {
                c.grants.reverse();
                if let Some(identity) = &mut c.identity {
                    identity.grants.reverse();
                }
            }
            for i in &mut t.indexes {
                i.options.reverse();
            }
            for p in &mut t.policies {
                p.roles.reverse();
            }
        }
        for s in &mut m.schemas {
            s.grants.reverse();
        }
        m.enums.reverse();
        m.sequences.reverse();
        for s in &mut m.sequences {
            s.grants.reverse();
        }
        m.roles.reverse();
        for r in &mut m.roles {
            r.member_of.reverse();
            r.other_attributes.reverse();
        }
        m.default_privileges.reverse();
        for d in &mut m.default_privileges {
            d.grants.reverse();
        }
        m.reported.reverse();
        m
    }

    #[test]
    fn model_serializes_deterministically() {
        let mut a = sample::full_model();
        let mut b = reversed(sample::full_model());
        assert_ne!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
        a.normalize();
        b.normalize();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );

        // Sorted where order carries no meaning …
        assert_eq!(a.schemas[0].name, "Billing");
        assert_eq!(a.tables[0].name, "my table");
        let orders = &a.tables[1];
        assert_eq!(orders.columns[0].attnum, 1);
        assert_eq!(
            orders.options,
            ["autovacuum_enabled=false", "fillfactor=70"]
        );
        assert_eq!(orders.policies[0].roles[0], RoleRef::Public);
        // Byte order: "Readers" before "app".
        assert_eq!(a.roles[0].name, "Readers");
        assert_eq!(a.roles[1].other_attributes, ["CREATEDB", "CREATEROLE"]);
        assert_eq!(a.roles[1].member_of[0].role.name, "neon_superuser");
        // … and left alone where it does.
        assert_eq!(a.enums[0].labels[0].label, "new");
        assert_eq!(orders.indexes[1].keys, ["status DESC", "lower(id::text)"]);
    }

    #[test]
    fn identity_sequence_grants_are_sorted() {
        let mut m = sample::full_model();
        m.normalize();
        let id = m.tables[1].columns[0].identity.as_ref().unwrap();
        assert_eq!(id.grants.len(), 2);
        // PUBLIC first, like every grant list.
        assert_eq!(id.grants[0].grantee, RoleRef::Public);
        assert_eq!(id.grants[1].privilege, "USAGE");
        let json = serde_json::to_value(id).unwrap();
        assert_eq!(json["grants"][0]["grantee"], "public");
    }

    #[test]
    fn memberships_with_several_grantors_sort_deterministically() {
        let mut a = sample::full_model();
        let mut b = sample::full_model();
        b.roles[0].member_of.reverse();
        a.normalize();
        b.normalize();
        assert_eq!(a.roles, b.roles);
        let app = &a.roles[1];
        let readers: Vec<&str> = app
            .member_of
            .iter()
            .filter(|m| m.role.name == "readers")
            .map(|m| m.grantor.name.as_str())
            .collect();
        assert_eq!(readers, ["postgres", "zed"]);
    }

    #[test]
    fn table_shallow_keeps_only_table_properties() {
        let t = &sample::full_model().tables[0];
        let s = t.shallow();
        assert!(s.columns.is_empty());
        assert!(s.constraints.is_empty());
        assert!(s.indexes.is_empty());
        assert!(s.policies.is_empty());
        assert!(s.grants.is_empty());
        let full = Table {
            columns: Vec::new(),
            constraints: Vec::new(),
            indexes: Vec::new(),
            policies: Vec::new(),
            grants: Vec::new(),
            ..t.clone()
        };
        assert_eq!(s, full);
        assert_eq!(s.options, t.options);
        assert!(s.rls_enabled && s.unlogged);
    }

    #[test]
    fn schema_sequence_and_column_shallow_drop_only_grants() {
        let m = sample::full_model();
        let s = &m.schemas[0];
        assert!(!s.grants.is_empty());
        assert_eq!(
            s.shallow(),
            Schema {
                grants: Vec::new(),
                ..s.clone()
            }
        );
        let q = &m.sequences[0];
        assert!(!q.grants.is_empty());
        assert_eq!(
            q.shallow(),
            Sequence {
                grants: Vec::new(),
                ..q.clone()
            }
        );
        let c = &m.tables[0].columns[1];
        assert!(!c.identity.as_ref().unwrap().grants.is_empty());
        let shallow = c.shallow();
        assert!(shallow.grants.is_empty());
        let id = shallow.identity.as_ref().unwrap();
        assert!(id.grants.is_empty());
        assert_eq!(id.params, c.identity.as_ref().unwrap().params);
        assert_eq!(shallow.name, c.name);
        let with_grants = &m.tables[0].columns[0];
        assert!(!with_grants.grants.is_empty());
        assert!(with_grants.shallow().grants.is_empty());
    }

    #[test]
    fn model_round_trips_through_json() {
        let mut m = sample::full_model();
        m.normalize();
        let json = serde_json::to_string(&m).unwrap();
        let back: SchemaModel = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    /// E29: a rename reaches every place a model names a role (checked on the serialized
    /// sample, which names `app` everywhere a role can be named).
    #[test]
    fn rename_roles_reaches_every_role_reference() {
        let m = sample::full_model();
        let before = serde_json::to_string(&m).unwrap();
        assert!(before.matches("\"app\"").count() > 10);
        let renamed = m.rename_roles(|oid, _| (oid == Oid(16400)).then(|| "web".to_string()));
        let after = serde_json::to_string(&renamed).unwrap();
        assert!(!after.contains("\"app\""), "{after}");
        assert_eq!(
            before.matches("\"app\"").count(),
            after.matches("\"web\"").count()
        );
        // By name too, and nothing else moves.
        let by_name = m.rename_roles(|_, name| (name == "app").then(|| "web".to_string()));
        assert_eq!(by_name, renamed);
        assert_eq!(m.rename_roles(|_, _| None), m);
    }

    #[test]
    fn owner_level_is_membership_in_neon_superuser() {
        let m = sample::full_model();
        assert!(m.roles[0].owner_level());
        assert!(!m.roles[1].owner_level());
    }

    /// TE-A P2: the table API answers with `Table` itself; its OpenAPI schema shows OIDs as
    /// plain integers, as the JSON carries them.
    #[cfg(feature = "openapi")]
    #[test]
    fn table_has_an_openapi_schema_with_integer_oids() {
        use utoipa::PartialSchema;
        let schema = serde_json::to_value(Table::schema()).unwrap();
        let props = &schema["properties"];
        assert_eq!(props["oid"]["type"], "integer", "{schema}");
        assert_eq!(props["oid"]["format"], "int32", "{schema}");
        for field in ["columns", "constraints", "indexes", "policies", "grants", "owner"] {
            assert!(props.get(field).is_some(), "{field}: {schema}");
        }
        let column = serde_json::to_value(Column::schema()).unwrap();
        assert_eq!(column["properties"]["type_oid"]["type"], "integer", "{column}");
    }
}
