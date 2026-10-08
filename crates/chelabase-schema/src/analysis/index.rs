//! One model's sub-objects, keyed by their stable [`ItemId`], each with its snapshot, a canonical
//! form for comparing across sides, and the references the merged model must resolve.
//!
//! **Ids.** An object with a base identity (through the identity map) is keyed by its base OID:
//! `table:<oid>`, `column:<table oid>:<attnum>`, `constraint:<oid>`, … An object newer than the
//! base is keyed by name under its container's id (names quoted as `quote_ident` does), so a
//! rename of the container on one side doesn't split it: `schema:name:<schema>`,
//! `table:name:<schema id>/<table>` (also enums and sequences), and a table's sub-object
//! `<kind>:name:<table id>/<name>`, e.g. `table:name:schema:2200/audit`,
//! `column:name:table:16420/email`, `index:name:table:name:schema:2200/audit/audit_idx`. Grants
//! are `grant:<object id>/<grantee>/<privilege>`, plus `/by:<grantor>` when the grantor isn't
//! the object's owner (an identity sequence's object id is its column's id plus `/identity`);
//! default privileges are `default_privilege:<role>/<schema id or *>/<objects>`.
//!
//! **References.** Every entry references its container (the parent id), and what it names:
//! owners, grantees, policy roles, memberships, a constraint's own columns, an FK's table and
//! columns, owned-by columns, an identity sequence, the columns an index or a policy or a
//! generated expression mentions (found with pg_query), and an enum column type. The ones that
//! appear in a definition text (`text_refs`) must also keep their name: the text was written
//! with it, and it isn't rewritten.
//!
//! **Canonical forms.** A snapshot carries one side's OIDs, which mean nothing on the other side.
//! Its canonical form replaces every OID with the base OID it maps to (or 0), and every name that
//! only refers to an object with a base identity (a table's schema, an owner, a grantee, an FK's
//! table) with that identity, so a rename on one side doesn't show up as a change of the objects
//! that merely refer to it. Two snapshots are the same change when their canonical forms are
//! equal. Definition texts (constraints, index keys, expressions) are compared as Postgres prints
//! them, names included.

use std::collections::{BTreeMap, HashMap};

use super::{GrantObject, ItemId, ObjKey, ObjKind, ObjSnapshot};
use crate::identity::{IdentityMap, ObjAt, Side};
use crate::model::{
    Column, Constraint, ConstraintKind, DefaultPrivilege, DefaultPrivilegeObject, EnumLabel,
    EnumType, ForeignKeyTarget, Grant, Index, Membership, NamedRole, ObjClass, Oid, Policy, Role,
    OWNER_LEVEL_ROLE, RoleRef, Schema, SchemaModel, Sequence, Table, membership_merges,
};
use crate::plan::OWNER_ROLE;
use crate::sql::{ident, qualified};

/// Which model a context reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Who {
    Base,
    Side(Side),
}

/// Lookups into one model, and its objects' base identities.
pub(crate) struct Ctx<'a> {
    pub model: &'a SchemaModel,
    pub who: Who,
    pub map: &'a IdentityMap,
    roles: HashMap<Oid, &'a Role>,
    schemas: HashMap<&'a str, &'a Schema>,
    tables: HashMap<Oid, &'a Table>,
    enums: HashMap<Oid, &'a EnumType>,
}

impl<'a> Ctx<'a> {
    pub fn new(model: &'a SchemaModel, who: Who, map: &'a IdentityMap) -> Self {
        Ctx {
            model,
            who,
            map,
            roles: model.roles.iter().map(|r| (r.oid, r)).collect(),
            schemas: model.schemas.iter().map(|s| (s.name.as_str(), s)).collect(),
            tables: model.tables.iter().map(|t| (t.oid, t)).collect(),
            enums: model.enums.iter().map(|e| (e.oid, e)).collect(),
        }
    }

    /// The base identity of this model's object. In the base itself, an object has one when
    /// the map records it (a base the map doesn't cover is matched by name, like a re-baseline).
    pub fn base_of(
        &self,
        class: ObjClass,
        oid: Oid,
        sub: Option<i16>,
    ) -> Option<(Oid, Option<i16>)> {
        match self.who {
            Who::Base => {
                let key = (class, ObjAt::new(oid, sub));
                self.map
                    .pairs()
                    .binary_search_by_key(&key, |p| (p.class, p.base))
                    .ok()
                    .map(|_| (oid, sub))
            }
            Who::Side(side) => self.map.to_base(side, class, oid, sub),
        }
    }

    fn base_oid(&self, class: ObjClass, oid: Oid) -> Option<Oid> {
        self.base_of(class, oid, None).map(|(o, _)| o)
    }

    pub fn has_table(&self, oid: Oid) -> bool {
        self.tables.contains_key(&oid)
    }

    pub fn table(&self, oid: Oid) -> Option<&'a Table> {
        self.tables.get(&oid).copied()
    }

    pub fn schema_id(&self, name: &str) -> ItemId {
        match self
            .schemas
            .get(name)
            .and_then(|s| self.base_oid(ObjClass::Schema, s.oid))
        {
            Some(b) => ItemId(format!("schema:{}", b.0)),
            None => ItemId(format!("schema:name:{}", ident(name))),
        }
    }

    /// The schema's id when this model has it (objects in an unmodelled schema have no parent).
    fn schema_parent(&self, name: &str) -> Option<ItemId> {
        self.schemas
            .contains_key(name)
            .then(|| self.schema_id(name))
    }

    /// A table by OID, falling back to the given names for a table this model doesn't have.
    pub fn table_id(&self, oid: Oid, schema: &str, name: &str) -> ItemId {
        match self.tables.get(&oid) {
            Some(t) => self.top_id("table", ObjClass::Table, oid, &t.schema, &t.name),
            None => self.named_id("table", schema, name),
        }
    }

    /// A newer top-level object: by its schema's id and its own name.
    fn named_id(&self, kind: &str, schema: &str, name: &str) -> ItemId {
        ItemId(format!(
            "{kind}:name:{}/{}",
            self.schema_id(schema).0,
            ident(name)
        ))
    }

    /// A column by its table's OID and `attnum`; `fallback` names it when this model doesn't.
    pub fn column_id(
        &self,
        table_oid: Oid,
        attnum: i16,
        schema: &str,
        table: &str,
        fallback: &str,
    ) -> ItemId {
        if let Some((bt, Some(ba))) = self.base_of(ObjClass::Column, table_oid, Some(attnum)) {
            return ItemId(format!("column:{}:{}", bt.0, ba));
        }
        let name = self
            .tables
            .get(&table_oid)
            .and_then(|t| t.columns.iter().find(|c| c.attnum == attnum))
            .map_or(fallback, |c| c.name.as_str());
        ItemId(format!(
            "column:name:{}/{}",
            self.table_id(table_oid, schema, table).0,
            ident(name)
        ))
    }

    fn child_id(
        &self,
        kind: &str,
        class: ObjClass,
        oid: Oid,
        table: &ItemId,
        name: &str,
    ) -> ItemId {
        match self.base_oid(class, oid) {
            Some(b) => ItemId(format!("{kind}:{}", b.0)),
            None => ItemId(format!("{kind}:name:{}/{}", table.0, ident(name))),
        }
    }

    fn top_id(&self, kind: &str, class: ObjClass, oid: Oid, schema: &str, name: &str) -> ItemId {
        match self.base_oid(class, oid) {
            Some(b) => ItemId(format!("{kind}:{}", b.0)),
            None => self.named_id(kind, schema, name),
        }
    }

    pub fn enum_id(&self, oid: Oid) -> Option<ItemId> {
        self.enums
            .get(&oid)
            .map(|e| self.top_id("enum", ObjClass::Enum, e.oid, &e.schema, &e.name))
    }

    /// A modelled role's id; `None` for a role outside the model (excluded or built in), which
    /// exists on every side.
    pub fn role_id(&self, oid: Oid) -> Option<ItemId> {
        self.roles
            .get(&oid)
            .map(|r| match self.base_oid(ObjClass::Role, oid) {
                Some(b) => ItemId(format!("role:{}", b.0)),
                None => ItemId(format!("role:name:{}", ident(&r.name))),
            })
    }

    fn role_token(&self, oid: Oid, name: &str) -> String {
        self.role_id(oid)
            .map_or_else(|| format!("role:name:{}", ident(name)), |id| id.0)
    }

    // ---- canonical forms ----

    fn canon_oid(&self, class: ObjClass, oid: Oid) -> Oid {
        self.base_oid(class, oid).unwrap_or(Oid(0))
    }

    fn canon_role(&self, r: &NamedRole) -> NamedRole {
        match self
            .roles
            .contains_key(&r.oid)
            .then(|| self.base_oid(ObjClass::Role, r.oid))
            .flatten()
        {
            Some(b) => NamedRole {
                oid: b,
                name: String::new(),
            },
            None => NamedRole {
                oid: Oid(0),
                name: r.name.clone(),
            },
        }
    }

    fn canon_role_ref(&self, r: &RoleRef) -> RoleRef {
        match r {
            RoleRef::Public => RoleRef::Public,
            RoleRef::Role { oid, name } => {
                let c = self.canon_role(&NamedRole {
                    oid: *oid,
                    name: name.clone(),
                });
                RoleRef::Role {
                    oid: c.oid,
                    name: c.name,
                }
            }
        }
    }

    fn canon_schema(&self, name: &str) -> String {
        match self
            .schemas
            .get(name)
            .and_then(|s| self.base_oid(ObjClass::Schema, s.oid))
        {
            Some(b) => format!("#{}", b.0),
            None => name.to_string(),
        }
    }

    fn canon_attnum(&self, table_oid: Oid, attnum: i16) -> i16 {
        match self.base_of(ObjClass::Column, table_oid, Some(attnum)) {
            Some((_, Some(a))) => a,
            _ => -1,
        }
    }

    fn canon_grant(&self, g: &Grant) -> Grant {
        Grant {
            grantor: self.canon_role(&g.grantor),
            grantee: self.canon_role_ref(&g.grantee),
            privilege: g.privilege.clone(),
            grantable: g.grantable,
        }
    }

    fn canon_fk(&self, r: &ForeignKeyTarget) -> ForeignKeyTarget {
        let base = self.base_oid(ObjClass::Table, r.table_oid);
        ForeignKeyTarget {
            table_oid: base.unwrap_or(Oid(0)),
            schema: self.canon_schema(&r.schema),
            table: if base.is_some() {
                String::new()
            } else {
                r.table.clone()
            },
            columns: r
                .columns
                .iter()
                .map(|a| self.canon_attnum(r.table_oid, *a))
                .collect(),
        }
    }

    fn canon_column(&self, table_oid: Oid, c: &Column) -> Column {
        let mut out = c.shallow();
        out.attnum = self.canon_attnum(table_oid, c.attnum);
        match self
            .enums
            .contains_key(&c.type_oid)
            .then(|| self.base_oid(ObjClass::Enum, c.type_oid))
            .flatten()
        {
            Some(b) => {
                out.type_oid = b;
                out.data_type = format!("#enum{}", b.0);
            }
            None => out.type_oid = Oid(0),
        }
        if let Some(identity) = &mut out.identity {
            identity.sequence_oid = Oid(0);
        }
        out
    }

    fn canon_labels(&self, labels: &[EnumLabel]) -> Vec<EnumLabel> {
        labels
            .iter()
            .map(|l| EnumLabel {
                oid: self.canon_oid(ObjClass::EnumLabel, l.oid),
                label: l.label.clone(),
            })
            .collect()
    }
}

/// One sub-object as one model has it.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub id: ItemId,
    pub key: ObjKey,
    /// The containing object's id (a table's schema, a column's table, a grant's object).
    pub parent: Option<ItemId>,
    pub snap: ObjSnapshot,
    pub canon: ObjSnapshot,
    /// Ids that must exist in the merged model when this object does (its parent included).
    pub refs: Vec<ItemId>,
    /// The part of `refs` this object's definition text names: when the object is taken from
    /// one side, those must have the same name in the merged model as on that side.
    pub text_refs: Vec<ItemId>,
    /// Statements wrapping this object's expressions, parsed to find the functions they call.
    pub exprs: Vec<String>,
    /// A role's parts the merge never makes (E30), compared across sides to report them.
    pub unmerged: Option<Unmerged>,
}

/// A role's parts outside U2 (E30), in canonical terms (roles by base identity).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Unmerged {
    /// `SUPERUSER`, `CREATEDB`, `CREATEROLE`, `REPLICATION`, sorted.
    pub other: Vec<String>,
    pub comment: Option<String>,
    /// Memberships the roles path refuses (`pg_*` and reserved roles), with their options.
    pub refused: Vec<String>,
    /// The options of each membership the roles path makes, by role: only a role both sides
    /// have is compared (a new membership gets the roles path's options).
    pub options: BTreeMap<String, Vec<String>>,
    /// The owner's U2 attributes (the owner is never merged).
    pub attributes: Option<String>,
}

impl Unmerged {
    /// Whether `self` (one side) and `other` differ in a part the merge would have to report.
    pub fn differs(&self, other: &Unmerged) -> bool {
        self.other != other.other
            || self.comment != other.comment
            || self.refused != other.refused
            || self.attributes != other.attributes
            || self
                .options
                .iter()
                .any(|(role, o)| other.options.get(role).is_some_and(|x| x != o))
    }
}

/// One model's entries by id.
pub(crate) struct SideIndex {
    pub entries: BTreeMap<ItemId, Entry>,
}

impl SideIndex {
    pub fn get(&self, id: &ItemId) -> Option<&Entry> {
        self.entries.get(id)
    }
}

/// The pseudo id that exists in the merged model when a column has an identity sequence.
pub(crate) fn identity_id(column: &ItemId) -> ItemId {
    ItemId(format!("{}/identity", column.0))
}

/// The name a definition text uses for an object: qualified for tables, enums and sequences
/// (texts read with `search_path = pg_catalog` qualify them), plain otherwise.
pub(crate) fn name_of(s: &ObjSnapshot) -> String {
    match s {
        ObjSnapshot::Schema(x) => x.name.clone(),
        ObjSnapshot::Table(x) => qualified(&x.schema, &x.name),
        ObjSnapshot::Column { column, .. } => column.name.clone(),
        ObjSnapshot::Constraint { constraint, .. } => constraint.name.clone(),
        ObjSnapshot::Index { index, .. } => index.name.clone(),
        ObjSnapshot::Policy { policy, .. } => policy.name.clone(),
        ObjSnapshot::Enum(x) => qualified(&x.schema, &x.name),
        ObjSnapshot::Sequence(x) => qualified(&x.schema, &x.name),
        ObjSnapshot::Role(x) => x.name.clone(),
        ObjSnapshot::Grant { .. } | ObjSnapshot::DefaultPrivilege(_) => String::new(),
    }
}

/// The column names an SQL statement mentions (column references, and plain index columns
/// including INCLUDE ones), found with pg_query. Unparsable text gives none (the rehearsal is
/// the final check).
pub(crate) fn named_columns(statement: &str) -> Vec<String> {
    use pg_query::{NodeEnum, NodeRef};
    let Ok(parsed) = pg_query::parse(statement) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (node, ..) in parsed.protobuf.nodes() {
        match node {
            NodeRef::ColumnRef(c) => {
                if let Some(Some(NodeEnum::String(s))) = c.fields.last().map(|f| f.node.as_ref()) {
                    out.push(s.sval.clone());
                }
            }
            NodeRef::IndexElem(e) if !e.name.is_empty() => out.push(e.name.clone()),
            _ => {}
        }
    }
    for stmt in &parsed.protobuf.stmts {
        if let Some(NodeEnum::IndexStmt(i)) = stmt.stmt.as_ref().and_then(|s| s.node.as_ref()) {
            for p in &i.index_including_params {
                if let Some(NodeEnum::IndexElem(e)) = &p.node {
                    out.push(e.name.clone());
                }
            }
        }
    }
    out
}

/// Parsable statements for an index's parts, one per key, per INCLUDE column and for the
/// predicate (its table and method don't matter here), so a part that doesn't parse loses only
/// its own references (T14 M3).
fn index_statements(i: &Index) -> Vec<String> {
    let mut out: Vec<String> = i
        .keys
        .iter()
        .chain(&i.include)
        .map(|k| format!("CREATE INDEX x ON t ({k})"))
        .collect();
    if let Some(p) = &i.predicate {
        out.push(format!("SELECT {p}"));
    }
    out
}

/// `CHECK (expr) [NOT VALID]` as `SELECT (expr)`.
fn check_expression(definition: &str) -> String {
    let body = definition.strip_prefix("CHECK ").unwrap_or(definition);
    let body = body.strip_suffix(" NOT VALID").unwrap_or(body);
    format!("SELECT {body}")
}

fn key(kind: ObjKind, schema: Option<&str>, table: Option<&str>, name: &str) -> ObjKey {
    ObjKey {
        kind,
        schema: schema.map(str::to_string),
        table: table.map(str::to_string),
        name: name.to_string(),
    }
}

fn object_type_name(o: DefaultPrivilegeObject) -> &'static str {
    match o {
        DefaultPrivilegeObject::Tables => "tables",
        DefaultPrivilegeObject::Sequences => "sequences",
        DefaultPrivilegeObject::Functions => "functions",
        DefaultPrivilegeObject::Types => "types",
        DefaultPrivilegeObject::Schemas => "schemas",
    }
}

struct Builder<'c, 'a> {
    c: &'c Ctx<'a>,
    entries: BTreeMap<ItemId, Entry>,
}

impl Builder<'_, '_> {
    /// Adds an entry; its parent is one of its references, and every text reference is one too.
    fn push(&mut self, mut e: Entry) {
        if let Some(p) = &e.parent {
            e.refs.push(p.clone());
        }
        e.refs.extend(e.text_refs.iter().cloned());
        e.refs.sort();
        e.refs.dedup();
        e.text_refs.sort();
        e.text_refs.dedup();
        self.entries.insert(e.id.clone(), e);
    }

    /// The columns of `t` this model has under the given names, as ids.
    fn columns_named(&self, t: &Table, names: &[String]) -> Vec<ItemId> {
        t.columns
            .iter()
            .filter(|col| names.contains(&col.name))
            .map(|col| {
                self.c
                    .column_id(t.oid, col.attnum, &t.schema, &t.name, &col.name)
            })
            .collect()
    }

    fn role_refs(&self, refs: &mut Vec<ItemId>, roles: impl IntoIterator<Item = Oid>) {
        refs.extend(roles.into_iter().filter_map(|oid| self.c.role_id(oid)));
    }

    /// One entry per grant on an object. `label` names the object in the key.
    #[allow(clippy::too_many_arguments)]
    fn grants(
        &mut self,
        on: &GrantObject,
        on_id: &ItemId,
        parent: &ItemId,
        schema: Option<&str>,
        table: Option<&str>,
        label: &str,
        owner: &NamedRole,
        grants: &[Grant],
        extra_ref: Option<ItemId>,
    ) {
        for g in grants {
            let (grantee, grantee_name) = match &g.grantee {
                RoleRef::Public => ("public".to_string(), "PUBLIC"),
                RoleRef::Role { oid, name } => (self.c.role_token(*oid, name), name.as_str()),
            };
            let mut id = ItemId(format!("grant:{}/{}/{}", on_id.0, grantee, g.privilege));
            if g.grantor.oid != owner.oid {
                // Granted by someone else than the owner: the same privilege can come from
                // several grantors, so the grantor is part of the id.
                id.0.push_str("/by:");
                id.0.push_str(&self.c.role_token(g.grantor.oid, &g.grantor.name));
            }
            let mut refs = Vec::new();
            if let RoleRef::Role { oid, .. } = &g.grantee {
                self.role_refs(&mut refs, [*oid]);
            }
            refs.extend(extra_ref.clone());
            self.push(Entry {
                key: key(
                    ObjKind::Grant,
                    schema,
                    table,
                    &format!("{} on {label} to {grantee_name}", g.privilege),
                ),
                id,
                parent: Some(parent.clone()),
                snap: ObjSnapshot::Grant {
                    on: on.clone(),
                    grant: g.clone(),
                },
                canon: ObjSnapshot::Grant {
                    on: GrantObject::Schema {
                        name: String::new(),
                    },
                    grant: self.c.canon_grant(g),
                },
                refs,
                text_refs: Vec::new(),
                exprs: Vec::new(),
                unmerged: None,
            });
        }
    }

    fn schema(&mut self, s: &Schema) {
        let c = self.c;
        let id = c.schema_id(&s.name);
        let mut refs = Vec::new();
        self.role_refs(&mut refs, [s.owner.oid]);
        self.push(Entry {
            id: id.clone(),
            key: key(ObjKind::Schema, None, None, &s.name),
            parent: None,
            snap: ObjSnapshot::schema(s),
            canon: ObjSnapshot::Schema(Schema {
                oid: c.canon_oid(ObjClass::Schema, s.oid),
                name: s.name.clone(),
                owner: c.canon_role(&s.owner),
                comment: s.comment.clone(),
                grants: Vec::new(),
            }),
            refs,
            text_refs: Vec::new(),
            exprs: Vec::new(),
            unmerged: None,
        });
        let on = GrantObject::Schema {
            name: s.name.clone(),
        };
        let label = format!("schema {}", s.name);
        self.grants(
            &on,
            &id,
            &id,
            Some(&s.name),
            None,
            &label,
            &s.owner,
            &s.grants,
            None,
        );
    }

    fn table(&mut self, t: &Table) {
        let c = self.c;
        let tid = c.table_id(t.oid, &t.schema, &t.name);
        let mut refs = Vec::new();
        self.role_refs(&mut refs, [t.owner.oid]);
        let mut canon = t.shallow();
        canon.oid = c.canon_oid(ObjClass::Table, t.oid);
        canon.schema = c.canon_schema(&t.schema);
        canon.owner = c.canon_role(&t.owner);
        self.push(Entry {
            id: tid.clone(),
            key: key(ObjKind::Table, Some(&t.schema), None, &t.name),
            parent: c.schema_parent(&t.schema),
            snap: ObjSnapshot::table(t),
            canon: ObjSnapshot::Table(canon),
            refs,
            text_refs: Vec::new(),
            exprs: Vec::new(),
            unmerged: None,
        });
        let on = GrantObject::Table {
            schema: t.schema.clone(),
            name: t.name.clone(),
        };
        let label = format!("table {}", t.name);
        self.grants(
            &on,
            &tid,
            &tid,
            Some(&t.schema),
            Some(&t.name),
            &label,
            &t.owner,
            &t.grants,
            None,
        );

        for col in &t.columns {
            self.column(t, &tid, col);
        }
        for k in &t.constraints {
            self.constraint(t, &tid, k);
        }
        for i in &t.indexes {
            self.index(t, &tid, i);
        }
        for p in &t.policies {
            self.policy(t, &tid, p);
        }
    }

    fn column(&mut self, t: &Table, tid: &ItemId, col: &Column) {
        let c = self.c;
        let cid = c.column_id(t.oid, col.attnum, &t.schema, &t.name, &col.name);
        // The type's name is in `data_type`; a generated expression names other columns.
        let mut text_refs: Vec<ItemId> = c.enum_id(col.type_oid).into_iter().collect();
        if let Some(g) = &col.generated {
            text_refs.extend(
                self.columns_named(t, &named_columns(&format!("SELECT {g}")))
                    .into_iter()
                    .filter(|id| *id != cid),
            );
        }
        let exprs = [&col.default, &col.generated]
            .into_iter()
            .flatten()
            .map(|e| format!("SELECT {e}"))
            .collect();
        self.push(Entry {
            id: cid.clone(),
            key: key(ObjKind::Column, Some(&t.schema), Some(&t.name), &col.name),
            parent: Some(tid.clone()),
            snap: ObjSnapshot::Column {
                table_oid: t.oid,
                schema: t.schema.clone(),
                table: t.name.clone(),
                column: col.shallow(),
            },
            canon: ObjSnapshot::Column {
                table_oid: Oid(0),
                schema: String::new(),
                table: String::new(),
                column: c.canon_column(t.oid, col),
            },
            refs: Vec::new(),
            text_refs,
            exprs,
            unmerged: None,
        });
        let on = GrantObject::Column {
            schema: t.schema.clone(),
            table: t.name.clone(),
            column: col.name.clone(),
        };
        let label = format!("column {}.{}", t.name, col.name);
        self.grants(
            &on,
            &cid,
            &cid,
            Some(&t.schema),
            Some(&t.name),
            &label,
            &t.owner,
            &col.grants,
            None,
        );
        if let Some(identity) = &col.identity {
            let on = GrantObject::Sequence {
                schema: t.schema.clone(),
                name: identity.sequence_name.clone(),
            };
            let label = format!("sequence {}", identity.sequence_name);
            let on_id = identity_id(&cid);
            self.grants(
                &on,
                &on_id,
                &cid,
                Some(&t.schema),
                Some(&t.name),
                &label,
                &t.owner,
                &identity.grants,
                Some(on_id.clone()),
            );
        }
    }

    fn constraint(&mut self, t: &Table, tid: &ItemId, k: &Constraint) {
        let c = self.c;
        let id = c.child_id("constraint", ObjClass::Constraint, k.oid, tid, &k.name);
        // Everything a constraint references is named in its definition text.
        let mut text_refs: Vec<ItemId> = k
            .columns
            .iter()
            .map(|a| c.column_id(t.oid, *a, &t.schema, &t.name, ""))
            .collect();
        if let Some(r) = &k.references
            && c.has_table(r.table_oid)
        {
            text_refs.push(c.table_id(r.table_oid, &r.schema, &r.table));
            for a in &r.columns {
                text_refs.push(c.column_id(r.table_oid, *a, &r.schema, &r.table, ""));
            }
        }
        let exprs = match k.kind {
            ConstraintKind::Check => vec![check_expression(&k.definition)],
            _ => Vec::new(),
        };
        self.push(Entry {
            id,
            key: key(ObjKind::Constraint, Some(&t.schema), Some(&t.name), &k.name),
            parent: Some(tid.clone()),
            snap: ObjSnapshot::Constraint {
                table_oid: t.oid,
                schema: t.schema.clone(),
                table: t.name.clone(),
                constraint: k.clone(),
            },
            canon: ObjSnapshot::Constraint {
                table_oid: Oid(0),
                schema: String::new(),
                table: String::new(),
                constraint: Constraint {
                    oid: c.canon_oid(ObjClass::Constraint, k.oid),
                    columns: k
                        .columns
                        .iter()
                        .map(|a| c.canon_attnum(t.oid, *a))
                        .collect(),
                    references: k.references.as_ref().map(|r| c.canon_fk(r)),
                    ..k.clone()
                },
            },
            refs: Vec::new(),
            text_refs,
            exprs,
            unmerged: None,
        });
    }

    fn index(&mut self, t: &Table, tid: &ItemId, i: &Index) {
        let c = self.c;
        let id = c.child_id("index", ObjClass::Index, i.oid, tid, &i.name);
        // Keys, INCLUDE columns and the predicate name columns only in text.
        let names: Vec<String> = index_statements(i)
            .iter()
            .flat_map(|s| named_columns(s))
            .collect();
        let text_refs = self.columns_named(t, &names);
        self.push(Entry {
            id,
            key: key(ObjKind::Index, Some(&t.schema), Some(&t.name), &i.name),
            parent: Some(tid.clone()),
            snap: ObjSnapshot::Index {
                table_oid: t.oid,
                schema: t.schema.clone(),
                table: t.name.clone(),
                index: i.clone(),
            },
            canon: ObjSnapshot::Index {
                table_oid: Oid(0),
                schema: String::new(),
                table: String::new(),
                // T14 I4: validity isn't a definition (an INVALID source index is reported and
                // never merged; a target's is the target's own business).
                index: Index {
                    oid: c.canon_oid(ObjClass::Index, i.oid),
                    valid: true,
                    ..i.clone()
                },
            },
            refs: Vec::new(),
            text_refs,
            exprs: Vec::new(),
            unmerged: None,
        });
    }

    fn policy(&mut self, t: &Table, tid: &ItemId, p: &Policy) {
        let c = self.c;
        let id = c.child_id("policy", ObjClass::Policy, p.oid, tid, &p.name);
        let mut refs = Vec::new();
        self.role_refs(
            &mut refs,
            p.roles.iter().filter_map(|r| match r {
                RoleRef::Public => None,
                RoleRef::Role { oid, .. } => Some(*oid),
            }),
        );
        let exprs: Vec<String> = [&p.using, &p.with_check]
            .into_iter()
            .flatten()
            .map(|e| format!("SELECT {e}"))
            .collect();
        // The table's columns the expressions name (roles are structured, not text).
        let names: Vec<String> = exprs.iter().flat_map(|e| named_columns(e)).collect();
        let text_refs = self.columns_named(t, &names);
        let mut roles: Vec<RoleRef> = p.roles.iter().map(|r| c.canon_role_ref(r)).collect();
        roles.sort_by_key(|r| format!("{r:?}"));
        self.push(Entry {
            id,
            key: key(ObjKind::Policy, Some(&t.schema), Some(&t.name), &p.name),
            parent: Some(tid.clone()),
            snap: ObjSnapshot::Policy {
                table_oid: t.oid,
                schema: t.schema.clone(),
                table: t.name.clone(),
                policy: p.clone(),
            },
            canon: ObjSnapshot::Policy {
                table_oid: Oid(0),
                schema: String::new(),
                table: String::new(),
                policy: Policy {
                    oid: c.canon_oid(ObjClass::Policy, p.oid),
                    roles,
                    ..p.clone()
                },
            },
            refs,
            text_refs,
            exprs,
            unmerged: None,
        });
    }

    fn enum_type(&mut self, e: &EnumType) {
        let c = self.c;
        let mut refs = Vec::new();
        self.role_refs(&mut refs, [e.owner.oid]);
        self.push(Entry {
            id: c.top_id("enum", ObjClass::Enum, e.oid, &e.schema, &e.name),
            key: key(ObjKind::Enum, Some(&e.schema), None, &e.name),
            parent: c.schema_parent(&e.schema),
            snap: ObjSnapshot::Enum(e.clone()),
            canon: ObjSnapshot::Enum(EnumType {
                oid: c.canon_oid(ObjClass::Enum, e.oid),
                schema: c.canon_schema(&e.schema),
                owner: c.canon_role(&e.owner),
                labels: c.canon_labels(&e.labels),
                ..e.clone()
            }),
            refs,
            text_refs: Vec::new(),
            exprs: Vec::new(),
            unmerged: None,
        });
    }

    fn sequence(&mut self, s: &Sequence) {
        let c = self.c;
        let id = c.top_id("sequence", ObjClass::Sequence, s.oid, &s.schema, &s.name);
        let mut refs = Vec::new();
        self.role_refs(&mut refs, [s.owner.oid]);
        let mut canon = s.shallow();
        canon.oid = c.canon_oid(ObjClass::Sequence, s.oid);
        canon.schema = c.canon_schema(&s.schema);
        canon.owner = c.canon_role(&s.owner);
        if let Some(o) = &s.owned_by {
            if c.has_table(o.table_oid) {
                refs.push(c.column_id(o.table_oid, o.attnum, &o.schema, &o.table, &o.column));
            }
            let co = canon.owned_by.as_mut().expect("cloned");
            let base = c.base_oid(ObjClass::Table, o.table_oid);
            co.table_oid = base.unwrap_or(Oid(0));
            co.schema = c.canon_schema(&o.schema);
            co.attnum = c.canon_attnum(o.table_oid, o.attnum);
            if base.is_some() {
                co.table = String::new();
            }
            if co.attnum >= 0 {
                co.column = String::new();
            }
        }
        self.push(Entry {
            id: id.clone(),
            key: key(ObjKind::Sequence, Some(&s.schema), None, &s.name),
            parent: c.schema_parent(&s.schema),
            snap: ObjSnapshot::sequence(s),
            canon: ObjSnapshot::Sequence(canon),
            refs,
            text_refs: Vec::new(),
            exprs: Vec::new(),
            unmerged: None,
        });
        let on = GrantObject::Sequence {
            schema: s.schema.clone(),
            name: s.name.clone(),
        };
        let label = format!("sequence {}", s.name);
        self.grants(
            &on,
            &id,
            &id,
            Some(&s.schema),
            None,
            &label,
            &s.owner,
            &s.grants,
            None,
        );
    }

    /// A role's canonical form holds only what the merge makes through the roles path (U2, E29,
    /// E30): the name, LOGIN, INHERIT, CONNECTION LIMIT, VALID UNTIL, BYPASSRLS and memberships
    /// by role (owner-level is the one in `neon_superuser`). Everything else is the role's
    /// `unmerged` form, reported "apply by hand" ([`super::roles`]), never merged: other
    /// attributes, the comment, memberships the roles path refuses (`pg_*` and reserved roles),
    /// the options of a membership, and every attribute of the owner ([`OWNER_ROLE`]), whose
    /// canonical form is its name only.
    fn role(&mut self, r: &Role) {
        let c = self.c;
        let mut refs = Vec::new();
        let owner = r.name == OWNER_ROLE;
        let canon_membership = |m: &Membership| Membership {
            role: c.canon_role(&m.role),
            grantor: NamedRole {
                oid: Oid(0),
                name: String::new(),
            },
            admin: false,
            inherit: false,
            set: false,
        };
        let mut canon = Role {
            oid: c.canon_oid(ObjClass::Role, r.oid),
            member_of: Vec::new(),
            other_attributes: Vec::new(),
            comment: None,
            ..r.clone()
        };
        if owner {
            // E30: never merged. E22: its memberships (Postgres's grants to it in the roles it
            // created) are never a change at all, nor a reference.
            canon = Role {
                oid: canon.oid,
                name: r.name.clone(),
                login: false,
                inherit: false,
                connection_limit: 0,
                valid_until: None,
                bypass_rls: false,
                member_of: Vec::new(),
                other_attributes: Vec::new(),
                comment: None,
            };
        } else {
            let merged = r.member_of.iter().filter(|m| membership_merges(&m.role.name));
            self.role_refs(&mut refs, merged.clone().map(|m| m.role.oid));
            canon.member_of = merged.map(canon_membership).collect();
            canon.member_of.sort_by_key(|m| format!("{m:?}"));
            canon.member_of.dedup();
        }
        let mut unmerged = Unmerged {
            other: r.other_attributes.clone(),
            comment: r.comment.clone(),
            refused: Vec::new(),
            options: BTreeMap::new(),
            attributes: owner.then(|| {
                format!(
                    "{:?}",
                    (
                        r.login,
                        r.inherit,
                        r.connection_limit,
                        &r.valid_until,
                        r.bypass_rls
                    )
                )
            }),
        };
        unmerged.other.sort();
        if !owner {
            for m in &r.member_of {
                let role = format!("{:?}", c.canon_role(&m.role));
                if m.role.name == OWNER_LEVEL_ROLE {
                    // Owner-level: the roles path's own grant, its options aren't the source's.
                    continue;
                }
                if membership_merges(&m.role.name) {
                    let options = format!("{:?}", (m.admin, m.inherit, m.set));
                    let slot = unmerged.options.entry(role).or_default();
                    slot.push(options);
                    slot.sort();
                    slot.dedup();
                } else {
                    unmerged
                        .refused
                        .push(format!("{role} {:?}", (m.admin, m.inherit, m.set)));
                }
            }
        }
        unmerged.refused.sort();
        unmerged.refused.dedup();
        self.push(Entry {
            unmerged: Some(unmerged),
            id: c.role_id(r.oid).expect("a modelled role"),
            key: key(ObjKind::Role, None, None, &r.name),
            parent: None,
            snap: ObjSnapshot::Role(r.clone()),
            canon: ObjSnapshot::Role(canon),
            refs,
            text_refs: Vec::new(),
            exprs: Vec::new(),
                    });
    }

    fn default_privilege(&mut self, d: &DefaultPrivilege) {
        let c = self.c;
        let schema = d
            .schema
            .as_deref()
            .map_or("*".to_string(), |s| c.schema_id(s).0);
        let id = ItemId(format!(
            "default_privilege:{}/{}/{}",
            c.role_token(d.role.oid, &d.role.name),
            schema,
            object_type_name(d.object_type)
        ));
        let mut refs = Vec::new();
        self.role_refs(&mut refs, [d.role.oid]);
        self.role_refs(
            &mut refs,
            d.grants.iter().filter_map(|g| match &g.grantee {
                RoleRef::Public => None,
                RoleRef::Role { oid, .. } => Some(*oid),
            }),
        );
        let mut grants: Vec<Grant> = d.grants.iter().map(|g| c.canon_grant(g)).collect();
        grants.sort_by_key(|g| format!("{g:?}"));
        self.push(Entry {
            id,
            key: key(
                ObjKind::DefaultPrivilege,
                d.schema.as_deref(),
                None,
                &format!("{} {}", d.role.name, object_type_name(d.object_type)),
            ),
            parent: d.schema.as_deref().and_then(|s| c.schema_parent(s)),
            snap: ObjSnapshot::DefaultPrivilege(d.clone()),
            canon: ObjSnapshot::DefaultPrivilege(DefaultPrivilege {
                oid: Oid(0),
                role: c.canon_role(&d.role),
                schema: d.schema.as_deref().map(|s| c.canon_schema(s)),
                object_type: d.object_type,
                grants,
            }),
            refs,
            text_refs: Vec::new(),
            exprs: Vec::new(),
            unmerged: None,
        });
    }
}

/// Every sub-object of the context's model.
pub(crate) fn build(c: &Ctx) -> SideIndex {
    let m = c.model;
    let mut b = Builder {
        c,
        entries: BTreeMap::new(),
    };
    for s in &m.schemas {
        b.schema(s);
    }
    for t in &m.tables {
        b.table(t);
    }
    for e in &m.enums {
        b.enum_type(e);
    }
    for s in &m.sequences {
        b.sequence(s);
    }
    for r in &m.roles {
        b.role(r);
    }
    for d in &m.default_privileges {
        b.default_privilege(d);
    }
    SideIndex { entries: b.entries }
}
