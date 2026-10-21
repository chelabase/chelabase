//! The catalog reader (T2): one SQL script that returns the database's catalog as one JSON
//! document, and its parser into a normalized [`SchemaModel`]. And the table API's scoped reads
//! (TE-A T4): one table ([`table_sql`]), the object list ([`objects_sql`]) and the type catalog
//! ([`types_sql`]), which run as the database owner (a non-superuser).

use chelabase_types::table_api::{ObjectKind, ObjectSummary, TypeEntry};
use serde::Deserialize;

use crate::error::SchemaError;
use crate::model::*;

/// Every read's start: `BEGIN … READ ONLY`, the `SET LOCAL`s, the users-table `DO` block and the
/// `WITH` list's shared CTEs (`users_tab`, `ext`, `ns`, `hidden`), ending in a comma.
const HEAD_SQL: &str = include_str!("catalog_parts/head.sql");
/// The `tabs` CTE without its closing parenthesis: the tables the model reads (the `tabs` rules).
const TABS_SQL: &str = include_str!("catalog_parts/tabs.sql");
/// One table's JSON object over `tabs t JOIN pg_roles o`: shared by [`CATALOG_SQL`]'s `tables`
/// and [`table_sql`].
const TABLE_OBJECT_SQL: &str = include_str!("catalog_parts/table.sql");
/// The object list's CTE and SELECT (after [`HEAD_SQL`] and the closed [`TABS_SQL`]).
const OBJECTS_SQL: &str = include_str!("catalog_parts/objects.sql");
/// The type catalog's CTE and SELECT (after [`HEAD_SQL`]).
const TYPES_SQL: &str = include_str!("catalog_parts/types.sql");

/// `BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL search_path = pg_catalog;
/// SET LOCAL DateStyle/TimeZone; DO <users table lookup>; SELECT <one json document>; COMMIT;`:
/// exactly one line of JSON on psql's `-X -q -A -t` stdout (every other statement prints
/// nothing under `-q`). Run as one simple query, the SELECT's single value is the document.
///
/// The P6 exclusions ([`crate::exclusions`]) are applied inside the SQL. The `DO` block reads the
/// users table named in `chelabase.app_users_table` only when that table exists (one SELECT
/// can't name a table that may be missing) and passes its OID on in a transaction-local
/// setting; it writes nothing, so it runs in the read-only transaction. A role that can't read
/// the pointer finds the users table from `pg_catalog` alone (its guard trigger), so the
/// owner-run reads below raise nothing and hide what this read hides.
pub const CATALOG_SQL: &str = concat!(
    include_str!("catalog_parts/head.sql"),
    include_str!("catalog_parts/tabs.sql"),
    "),\n",
    include_str!("catalog_sql.sql"),
    include_str!("catalog_parts/table.sql"),
    " AS j\n  FROM tabs t JOIN pg_roles o ON o.oid = t.relowner\n)\n",
    include_str!("catalog_parts/catalog_select.sql"),
);

/// A name as an SQL literal for the scoped reads. No Postgres name holds a NUL, so one that does
/// becomes `NULL`, which matches nothing (a NUL would cut the query text short).
fn name_literal(name: &str) -> String {
    if name.contains('\0') {
        "NULL".to_string()
    } else {
        crate::sql::literal(name)
    }
}

/// One table as [`CATALOG_SQL`] reads it (the same per-table object, the same `tabs` rules and
/// exclusions): `{"server_version_num": …, "table": <object or null>}`, `null` when `schema.name`
/// isn't a table the model reads. Its own read-only transaction (`BEGIN … COMMIT`), so it is never
/// run inside another. Runs as the database owner, a non-superuser: it reads `pg_catalog` only,
/// and the users-table pointer only when the role may ([`HEAD_SQL`]).
pub fn table_sql(schema: &str, name: &str) -> String {
    format!(
        "{HEAD_SQL}{TABS_SQL}    AND n.nspname = {} AND c.relname = {}\n),\n\
         tables_j AS (\n  SELECT {TABLE_OBJECT_SQL} AS j\n  FROM tabs t JOIN pg_roles o ON o.oid = t.relowner\n)\n\
         SELECT jsonb_build_object(\n  'server_version_num', current_setting('server_version_num')::int,\n  \
         'table', (SELECT j FROM tables_j)\n)::text;\nCOMMIT;\n",
        name_literal(schema),
        name_literal(name),
    )
}

/// Parses [`table_sql`]'s output (normalized as [`parse_catalog`] normalizes a table); refuses a
/// server major other than [`crate::SUPPORTED_SERVER_MAJOR`].
pub fn parse_table(json: &str) -> Result<Option<Table>, SchemaError> {
    let major = server_major(json)?;
    let raw: RawTableDocument = serde_json::from_str(json).map_err(parse_error)?;
    let Some(raw) = raw.table else {
        return Ok(None);
    };
    let mut m = SchemaModel::empty(major);
    m.tables.push(table(raw)?);
    m.normalize();
    Ok(m.tables.pop())
}

/// One read-only relation's object over `pg_class c JOIN ns n JOIN pg_roles o`, in
/// [`TABLE_OBJECT_SQL`]'s shape: its columns only, every other list empty. It reads `pg_class`,
/// `pg_attribute` and the columns' defaults and identity sequences, never a view's or rule's
/// definition or a foreign table's options.
const RELATION_OBJECT_SQL: &str = include_str!("catalog_parts/relation.sql");

/// One relation of any kind the object list shows ([`objects_sql`]):
/// `{"server_version_num": …, "kind": <ObjectKind or null>, "table": <object or null>}`.
///
/// - An ordinary table the model reads (`tabs`) is [`table_sql`]'s object, kind `table`.
/// - A table that inherits from another (which `tabs` leaves out, since the schema merge doesn't
///   model inheritance) is the same full object, kind `table`: its catalog rows describe it as
///   Postgres has it, inherited columns and CHECK constraints included.
/// - A partitioned table, view, materialized view or foreign table is
///   [`RELATION_OBJECT_SQL`]'s columns-only object.
///
/// Partitions, extension members and the schemas the model leaves out (`ns`) are `null`, as the
/// object list leaves them out. Its own read-only transaction, run as the database owner.
pub fn relation_sql(schema: &str, name: &str) -> String {
    let (schema, name) = (name_literal(schema), name_literal(name));
    format!(
        "{HEAD_SQL}{TABS_SQL}    AND n.nspname = {schema} AND c.relname = {name}\n),\n\
         inherits AS (\n  SELECT c.oid, c.relname, c.relowner, c.relacl, c.relrowsecurity, c.relforcerowsecurity,\n         \
         c.relpersistence, c.reloptions, n.nspname,\n         \
         (c.oid = (SELECT oid FROM users_tab)) IS TRUE AS is_users\n  \
         FROM pg_class c JOIN ns n ON n.oid = c.relnamespace\n  \
         WHERE c.relkind = 'r' AND NOT c.relispartition\n    \
         AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)\n    \
         AND EXISTS (SELECT FROM pg_inherits ih WHERE ih.inhrelid = c.oid)\n    \
         AND n.nspname = {schema} AND c.relname = {name}\n),\n\
         tables_j AS (\n  SELECT {TABLE_OBJECT_SQL} AS j\n  \
         FROM (SELECT * FROM tabs UNION ALL SELECT * FROM inherits) t JOIN pg_roles o ON o.oid = t.relowner\n),\n\
         others_j AS (\n  SELECT CASE c.relkind WHEN 'p' THEN 'partitioned_table'\n                        \
         WHEN 'v' THEN 'view'\n                        \
         WHEN 'm' THEN 'materialized_view'\n                        \
         WHEN 'f' THEN 'foreign_table' END AS kind,\n  \
         {RELATION_OBJECT_SQL} AS j\n  \
         FROM pg_class c\n  JOIN ns n ON n.oid = c.relnamespace\n  JOIN pg_roles o ON o.oid = c.relowner\n  \
         WHERE c.relkind IN ('p', 'v', 'm', 'f') AND NOT c.relispartition\n    \
         AND NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)\n    \
         AND n.nspname = {schema} AND c.relname = {name}\n)\n\
         SELECT jsonb_build_object(\n  'server_version_num', current_setting('server_version_num')::int,\n  \
         'kind', coalesce((SELECT 'table'::text FROM tables_j), (SELECT kind FROM others_j)),\n  \
         'table', coalesce((SELECT j FROM tables_j), (SELECT j FROM others_j))\n)::text;\nCOMMIT;\n",
    )
}

/// Parses [`relation_sql`]'s output; refuses a server major other than
/// [`crate::SUPPORTED_SERVER_MAJOR`] and a kind other than the object list's. An ordinary table
/// is normalized as [`parse_table`] normalizes it; every other kind keeps its columns only (its
/// constraints, indexes, policies, grants and options are emptied, whatever the document held).
pub fn parse_relation(json: &str) -> Result<Option<Relation>, SchemaError> {
    let major = server_major(json)?;
    let raw: RawRelationDocument = serde_json::from_str(json).map_err(parse_error)?;
    let (kind, raw) = match (raw.kind, raw.table) {
        (None, None) => return Ok(None),
        (Some(kind), Some(raw)) => (kind, raw),
        (Some(_), None) => return Err(SchemaError::Parse("a relation kind without its object".into())),
        (None, Some(_)) => return Err(SchemaError::Parse("a relation object without its kind".into())),
    };
    let mut table = table(raw)?;
    if kind != ObjectKind::Table {
        table.constraints.clear();
        table.indexes.clear();
        table.policies.clear();
        table.grants.clear();
        table.options.clear();
    }
    let mut m = SchemaModel::empty(major);
    m.tables.push(table);
    m.normalize();
    let table = m.tables.pop().expect("the one table pushed");
    Ok(Some(Relation { kind, table }))
}

/// The table API's object list: `{"server_version_num": …, "objects": [ObjectSummary…]}`. Runs as
/// the database owner.
pub fn objects_sql() -> String {
    format!("{HEAD_SQL}{TABS_SQL}),\n{OBJECTS_SQL}")
}

/// Parses [`objects_sql`]'s output, sorted by `(schema, name)`.
pub fn parse_objects(json: &str) -> Result<Vec<ObjectSummary>, SchemaError> {
    server_major(json)?;
    let raw: RawObjects = serde_json::from_str(json).map_err(parse_error)?;
    let mut objects = raw.objects;
    objects.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
    Ok(objects)
}

/// The table API's type catalog: `{"server_version_num": …, "types": [TypeEntry…]}`. Runs as the
/// database owner.
pub fn types_sql() -> String {
    format!("{HEAD_SQL}{TYPES_SQL}")
}

/// Parses [`types_sql`]'s output, sorted by `(schema, name)`.
pub fn parse_types(json: &str) -> Result<Vec<TypeEntry>, SchemaError> {
    server_major(json)?;
    let raw: RawTypes = serde_json::from_str(json).map_err(parse_error)?;
    let mut types = raw.types;
    types.sort_by(|a, b| (&a.schema, &a.name).cmp(&(&b.schema, &b.name)));
    Ok(types)
}

/// The document's server major, refused unless it is [`crate::SUPPORTED_SERVER_MAJOR`]; read
/// before anything else.
fn server_major(json: &str) -> Result<u32, SchemaError> {
    let version: RawVersion = serde_json::from_str(json).map_err(parse_error)?;
    let major = version.server_version_num / 10000;
    if major != crate::SUPPORTED_SERVER_MAJOR {
        return Err(SchemaError::UnsupportedServer(major));
    }
    Ok(major)
}

/// [`CATALOG_SQL`]'s filter on the data API's per-schema default-privilege grants to `anon` and
/// `authenticated` (P6), on the owner's rows only (T14 M5: a user's own `FOR ROLE other … TO
/// authenticated` row stays).
const DATA_API_DEFAULT_ACL_FILTER: &str = "\n              AND NOT (d.defaclnamespace <> 0 AND dr.rolname = 'postgres'\n                       AND coalesce(ge.rolname, '') IN ('anon', 'authenticated'))";

/// [`CATALOG_SQL`] keeping the data API's per-schema default-privilege rows: the `default_acl`
/// model [`crate::compare::expected_model`] simulates new objects' grants with (P7). The tables a
/// merge creates in a served schema get those grants, so the drift check must expect them. Only
/// its `default_privileges` are used.
pub fn default_acl_sql() -> String {
    CATALOG_SQL.replacen(DATA_API_DEFAULT_ACL_FILTER, "", 1)
}

/// Parses [`CATALOG_SQL`]'s output; refuses a server major other than
/// [`crate::SUPPORTED_SERVER_MAJOR`]; normalizes and sorts.
pub fn parse_catalog(json: &str) -> Result<SchemaModel, SchemaError> {
    let major = server_major(json)?;
    let raw: RawCatalog = serde_json::from_str(json).map_err(parse_error)?;
    let mut m = SchemaModel::empty(major);
    m.schemas = raw
        .schemas
        .into_iter()
        .map(|s| {
            Ok(Schema {
                oid: s.oid,
                name: s.name,
                owner: s.owner,
                comment: s.comment,
                grants: grants(s.acl)?,
            })
        })
        .collect::<Result<_, SchemaError>>()?;
    m.tables = raw
        .tables
        .into_iter()
        .map(table)
        .collect::<Result<_, _>>()?;
    m.enums = raw.enums;
    m.sequences = raw
        .sequences
        .into_iter()
        .map(|s| {
            Ok(Sequence {
                oid: s.oid,
                schema: s.schema,
                name: s.name,
                owner: s.owner,
                comment: s.comment,
                params: s.params,
                owned_by: s.owned_by,
                grants: grants(s.acl)?,
            })
        })
        .collect::<Result<_, SchemaError>>()?;
    m.roles = raw.roles;
    m.default_privileges = raw
        .default_privileges
        .into_iter()
        .map(|d| {
            Ok(DefaultPrivilege {
                oid: d.oid,
                role: d.role,
                schema: d.schema,
                object_type: match d.object_type.as_str() {
                    "r" => DefaultPrivilegeObject::Tables,
                    "S" => DefaultPrivilegeObject::Sequences,
                    "f" => DefaultPrivilegeObject::Functions,
                    "T" => DefaultPrivilegeObject::Types,
                    "n" => DefaultPrivilegeObject::Schemas,
                    other => return Err(unknown("defaclobjtype", other)),
                },
                grants: grants(d.acl)?,
            })
        })
        .collect::<Result<_, SchemaError>>()?;
    m.reported = raw
        .reported
        .into_iter()
        .map(|r| Reported {
            kind: reported_kind(r.kind),
            oid: r.oid,
            schema: r.schema,
            name: r.name,
            parent: r.parent,
            arguments: r.arguments,
            definition: r.definition,
        })
        .collect();
    m.normalize();
    Ok(m)
}

fn parse_error(e: serde_json::Error) -> SchemaError {
    SchemaError::Parse(e.to_string())
}

fn unknown(what: &str, value: &str) -> SchemaError {
    SchemaError::UnknownCatalogValue {
        what: what.to_string(),
        value: value.to_string(),
    }
}

fn reported_kind(kind: String) -> ReportedKind {
    match kind.as_str() {
        "view" => ReportedKind::View,
        "materialized_view" => ReportedKind::MaterializedView,
        "function" => ReportedKind::Function,
        "procedure" => ReportedKind::Procedure,
        "trigger" => ReportedKind::Trigger,
        "partitioned_table" => ReportedKind::PartitionedTable,
        "partition" => ReportedKind::Partition,
        "inherited_table" => ReportedKind::InheritedTable,
        "foreign_table" => ReportedKind::ForeignTable,
        "domain" => ReportedKind::Domain,
        "composite_type" => ReportedKind::CompositeType,
        "extension" => ReportedKind::Extension,
        "rule" => ReportedKind::Rule,
        "cast" => ReportedKind::Cast,
        "operator" => ReportedKind::Operator,
        "publication" => ReportedKind::Publication,
        "event_trigger" => ReportedKind::EventTrigger,
        "merge_leftover" => ReportedKind::MergeLeftover,
        _ => ReportedKind::Other(kind),
    }
}

fn grants(acl: Vec<RawGrant>) -> Result<Vec<Grant>, SchemaError> {
    acl.into_iter()
        .map(|g| {
            Ok(Grant {
                grantor: g.grantor,
                grantee: role_ref(g.grantee)?,
                privilege: g.privilege,
                grantable: g.grantable,
            })
        })
        .collect()
}

/// OID 0 is `PUBLIC` (`aclexplode`'s grantee, `polroles = {0}`).
fn role_ref(r: RawRoleRef) -> Result<RoleRef, SchemaError> {
    match (r.oid, r.name) {
        (Oid(0), _) => Ok(RoleRef::Public),
        (oid, Some(name)) => Ok(RoleRef::Role { oid, name }),
        (oid, None) => Err(SchemaError::Parse(format!("role {} has no name", oid.0))),
    }
}

fn table(t: RawTable) -> Result<Table, SchemaError> {
    let unlogged = match t.persistence.as_str() {
        "p" => false,
        "u" => true,
        other => return Err(unknown("relpersistence", other)),
    };
    Ok(Table {
        oid: t.oid,
        schema: t.schema,
        name: t.name,
        owner: t.owner,
        comment: t.comment,
        rls_enabled: t.rls_enabled,
        rls_forced: t.rls_forced,
        unlogged,
        options: t.options,
        columns: t
            .columns
            .into_iter()
            .map(column)
            .collect::<Result<_, _>>()?,
        constraints: t
            .constraints
            .into_iter()
            .map(constraint)
            .collect::<Result<_, _>>()?,
        indexes: t.indexes.into_iter().map(index).collect::<Result<_, _>>()?,
        policies: t
            .policies
            .into_iter()
            .map(policy)
            .collect::<Result<_, _>>()?,
        grants: grants(t.acl)?,
    })
}

fn column(c: RawColumn) -> Result<Column, SchemaError> {
    // A generated column's expression is its pg_attrdef row, not a default.
    let (default, generated) = match c.generated.as_str() {
        "" => (c.default_expr, None),
        "s" => (None, c.default_expr),
        other => return Err(unknown("attgenerated", other)),
    };
    let generation = match c.identity.as_str() {
        "" => None,
        "a" => Some(IdentityGeneration::Always),
        "d" => Some(IdentityGeneration::ByDefault),
        other => return Err(unknown("attidentity", other)),
    };
    let identity = match (generation, c.identity_sequence) {
        (None, _) => None,
        (Some(generation), Some(seq)) => Some(Identity {
            generation,
            sequence_oid: seq.oid,
            sequence_name: seq.name,
            params: seq.params,
            grants: grants(seq.acl)?,
        }),
        (Some(_), None) => {
            return Err(SchemaError::Parse(format!(
                "identity column {} has no sequence",
                c.name
            )));
        }
    };
    Ok(Column {
        attnum: c.attnum,
        name: c.name,
        data_type: c.data_type,
        type_oid: c.type_oid,
        collation: c.collation,
        default,
        not_null: c.not_null,
        identity,
        generated,
        comment: c.comment,
        grants: grants(c.acl)?,
    })
}

fn constraint(k: RawConstraint) -> Result<Constraint, SchemaError> {
    let kind = match k.contype.as_str() {
        "p" => ConstraintKind::PrimaryKey,
        "u" => ConstraintKind::Unique,
        "f" => ConstraintKind::ForeignKey,
        "c" => ConstraintKind::Check,
        "x" => ConstraintKind::Exclusion,
        other => return Err(unknown("contype", other)),
    };
    Ok(Constraint {
        oid: k.oid,
        name: k.name,
        kind,
        definition: k.definition,
        columns: k.columns,
        references: k.references,
        deferrable: k.deferrable,
        initially_deferred: k.initially_deferred,
        validated: k.validated,
    })
}

fn index(i: RawIndex) -> Result<Index, SchemaError> {
    let keys = index_keys(&i.definition).ok_or_else(|| {
        SchemaError::Parse(format!("unreadable index definition: {}", i.definition))
    })?;
    if keys.len() != i.key_count {
        return Err(SchemaError::Parse(format!(
            "index {} has {} key columns, its definition {}",
            i.name,
            i.key_count,
            keys.len()
        )));
    }
    Ok(Index {
        oid: i.oid,
        name: i.name,
        method: i.method,
        unique: i.unique,
        nulls_not_distinct: i.nulls_not_distinct,
        keys,
        include: i.include,
        predicate: i.predicate,
        options: i.options,
        valid: i.valid,
        comment: i.comment,
    })
}

fn policy(p: RawPolicy) -> Result<Policy, SchemaError> {
    let command = match p.command.as_str() {
        "*" => PolicyCommand::All,
        "r" => PolicyCommand::Select,
        "a" => PolicyCommand::Insert,
        "w" => PolicyCommand::Update,
        "d" => PolicyCommand::Delete,
        other => return Err(unknown("polcmd", other)),
    };
    Ok(Policy {
        oid: p.oid,
        name: p.name,
        command,
        permissive: p.permissive,
        roles: p
            .roles
            .into_iter()
            .map(role_ref)
            .collect::<Result<_, _>>()?,
        using: p.using,
        with_check: p.with_check,
        comment: p.comment,
    })
}

/// The key columns and expressions of `pg_get_indexdef(oid, 0, true)`, each as printed (with
/// collation, operator class and ordering): the first parenthesized list outside quotes, split
/// at its top-level commas. The index's and table's names come before it, so they're dropped.
fn index_keys(def: &str) -> Option<Vec<String>> {
    let mut keys = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut in_ident = false;
    let mut in_literal = false;
    for ch in def.chars() {
        if in_ident {
            in_ident = ch != '"';
        } else if in_literal {
            in_literal = ch != '\'';
        } else {
            match ch {
                '"' => in_ident = true,
                '\'' => in_literal = true,
                '(' => {
                    depth += 1;
                    if depth == 1 {
                        continue;
                    }
                }
                ')' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        keys.push(current.trim().to_string());
                        return Some(keys);
                    }
                }
                ',' if depth == 1 => {
                    keys.push(std::mem::take(&mut current).trim().to_string());
                    continue;
                }
                _ => {}
            }
        }
        if depth >= 1 {
            current.push(ch);
        }
    }
    None
}

// The document CATALOG_SQL prints. Codes stay raw ("char" values) so unknown ones are errors.

#[derive(Deserialize)]
struct RawVersion {
    server_version_num: u32,
}

#[derive(Deserialize)]
struct RawCatalog {
    schemas: Vec<RawSchema>,
    tables: Vec<RawTable>,
    enums: Vec<EnumType>,
    sequences: Vec<RawSequence>,
    roles: Vec<Role>,
    default_privileges: Vec<RawDefaultPrivilege>,
    reported: Vec<RawReported>,
}

#[derive(Deserialize)]
struct RawTableDocument {
    table: Option<RawTable>,
}

#[derive(Deserialize)]
struct RawRelationDocument {
    kind: Option<ObjectKind>,
    table: Option<RawTable>,
}

#[derive(Deserialize)]
struct RawObjects {
    objects: Vec<ObjectSummary>,
}

#[derive(Deserialize)]
struct RawTypes {
    types: Vec<TypeEntry>,
}

#[derive(Deserialize)]
struct RawRoleRef {
    oid: Oid,
    name: Option<String>,
}

#[derive(Deserialize)]
struct RawGrant {
    grantor: NamedRole,
    grantee: RawRoleRef,
    privilege: String,
    grantable: bool,
}

#[derive(Deserialize)]
struct RawSchema {
    oid: Oid,
    name: String,
    owner: NamedRole,
    comment: Option<String>,
    acl: Vec<RawGrant>,
}

#[derive(Deserialize)]
struct RawTable {
    oid: Oid,
    schema: String,
    name: String,
    owner: NamedRole,
    comment: Option<String>,
    rls_enabled: bool,
    rls_forced: bool,
    persistence: String,
    options: Vec<String>,
    acl: Vec<RawGrant>,
    columns: Vec<RawColumn>,
    constraints: Vec<RawConstraint>,
    indexes: Vec<RawIndex>,
    policies: Vec<RawPolicy>,
}

#[derive(Deserialize)]
struct RawColumn {
    attnum: i16,
    name: String,
    data_type: String,
    type_oid: Oid,
    collation: Option<String>,
    default_expr: Option<String>,
    generated: String,
    identity: String,
    not_null: bool,
    identity_sequence: Option<RawIdentitySequence>,
    comment: Option<String>,
    acl: Vec<RawGrant>,
}

#[derive(Deserialize)]
struct RawIdentitySequence {
    oid: Oid,
    name: String,
    params: SequenceParams,
    acl: Vec<RawGrant>,
}

#[derive(Deserialize)]
struct RawConstraint {
    oid: Oid,
    name: String,
    contype: String,
    definition: String,
    columns: Vec<i16>,
    references: Option<ForeignKeyTarget>,
    deferrable: bool,
    initially_deferred: bool,
    validated: bool,
}

#[derive(Deserialize)]
struct RawIndex {
    oid: Oid,
    name: String,
    method: String,
    unique: bool,
    nulls_not_distinct: bool,
    definition: String,
    key_count: usize,
    include: Vec<String>,
    predicate: Option<String>,
    options: Vec<String>,
    valid: bool,
    comment: Option<String>,
}

#[derive(Deserialize)]
struct RawPolicy {
    oid: Oid,
    name: String,
    command: String,
    permissive: bool,
    roles: Vec<RawRoleRef>,
    using: Option<String>,
    with_check: Option<String>,
    #[serde(default)]
    comment: Option<String>,
}

#[derive(Deserialize)]
struct RawSequence {
    oid: Oid,
    schema: String,
    name: String,
    owner: NamedRole,
    comment: Option<String>,
    params: SequenceParams,
    owned_by: Option<OwnedBy>,
    acl: Vec<RawGrant>,
}

#[derive(Deserialize)]
struct RawDefaultPrivilege {
    oid: Oid,
    role: NamedRole,
    schema: Option<String>,
    object_type: String,
    acl: Vec<RawGrant>,
}

#[derive(Deserialize)]
struct RawReported {
    kind: String,
    oid: Oid,
    schema: Option<String>,
    name: String,
    parent: Option<String>,
    arguments: Option<String>,
    definition: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exclusions;
    use serde_json::{Value, json};

    fn role(oid: u32, name: &str) -> Value {
        json!({ "oid": oid, "name": name })
    }

    fn grant(grantee: u32, grantee_name: Option<&str>, privilege: &str) -> Value {
        json!({
            "grantor": role(10, "postgres"),
            "grantee": { "oid": grantee, "name": grantee_name },
            "privilege": privilege,
            "grantable": false,
        })
    }

    fn column(attnum: i16, name: &str) -> Value {
        json!({
            "attnum": attnum, "name": name, "data_type": "integer", "type_oid": 23,
            "collation": null, "default_expr": null, "generated": "", "identity": "",
            "identity_sequence": null, "not_null": false, "comment": null, "acl": [],
        })
    }

    /// A raw document as [`CATALOG_SQL`] prints it, with its unordered lists out of order.
    fn document(server_version_num: u32) -> Value {
        let mut id = column(1, "id");
        id["identity"] = json!("a");
        id["not_null"] = json!(true);
        id["identity_sequence"] = json!({
            "oid": 16419, "name": "Order_id_seq",
            "params": { "data_type": "bigint", "start": 100, "increment": 5, "min": 1,
                        "max": i64::MAX, "cache": 3, "cycle": false },
            "acl": [grant(16400, Some("app"), "USAGE"), grant(0, None, "SELECT")],
        });
        let mut doubled = column(3, "doubled");
        doubled["generated"] = json!("s");
        doubled["default_expr"] = json!("(id * 2)");
        let mut status = column(2, "status");
        status["acl"] = json!([
            grant(16400, Some("app"), "UPDATE"),
            grant(16400, Some("app"), "SELECT")
        ]);
        json!({
            "server_version_num": server_version_num,
            "schemas": [
                { "oid": 2200, "name": "public", "owner": role(10, "postgres"), "comment": null,
                  "acl": [grant(10, Some("postgres"), "USAGE"), grant(0, None, "USAGE")] },
                { "oid": 16390, "name": "Billing", "owner": role(10, "postgres"), "comment": "money",
                  "acl": [] },
            ],
            "tables": [{
                "oid": 16420, "schema": "Billing", "name": "Order", "owner": role(10, "postgres"),
                "comment": null, "rls_enabled": true, "rls_forced": false, "persistence": "u",
                "options": ["fillfactor=70", "autovacuum_enabled=false"],
                "acl": [grant(16400, Some("app"), "SELECT"), grant(0, None, "INSERT")],
                "columns": [doubled, status, id],
                "constraints": [
                    { "oid": 16431, "name": "order_pkey", "contype": "p",
                      "definition": "PRIMARY KEY (id)", "columns": [1], "references": null,
                      "deferrable": false, "initially_deferred": false, "validated": true },
                    { "oid": 16430, "name": "fk", "contype": "f",
                      "definition": "FOREIGN KEY (status) REFERENCES t(id) NOT VALID",
                      "columns": [2],
                      "references": { "table_oid": 16500, "schema": "public", "table": "t",
                                      "columns": [1] },
                      "deferrable": false, "initially_deferred": false, "validated": false },
                ],
                "indexes": [{
                    "oid": 16440, "name": "ix(\"odd\")", "method": "btree", "unique": true,
                    "nulls_not_distinct": false,
                    "definition": "CREATE UNIQUE INDEX \"ix(\"\"odd\"\")\" ON \"Billing\".\"Order\" USING btree (lower((\"a,b\")::text) DESC, status text_pattern_ops, (id || ',('::text)) INCLUDE (id) WHERE id > 0",
                    "key_count": 3, "include": ["id"], "predicate": "id > 0",
                    "options": ["fillfactor=80", "deduplicate_items=off"], "valid": false,
                    "comment": null,
                }],
                "policies": [{
                    "oid": 16450, "name": "own rows", "command": "w", "permissive": false,
                    "roles": [role(16400, "app"), { "oid": 0, "name": null }],
                    "using": "(id = 1)", "with_check": "(id = 2)",
                }],
            }],
            "enums": [{
                "oid": 16391, "schema": "Billing", "name": "status", "owner": role(10, "postgres"),
                "comment": null,
                "labels": [{ "oid": 16393, "label": "new" }, { "oid": 16392, "label": "gone" }],
            }],
            "sequences": [{
                "oid": 16460, "schema": "public", "name": "tickets", "owner": role(10, "postgres"),
                "comment": null,
                "params": { "data_type": "integer", "start": 7, "increment": 1, "min": 1,
                            "max": 2147483647, "cache": 1, "cycle": true },
                "owned_by": { "table_oid": 16420, "schema": "Billing", "table": "Order",
                              "attnum": 1, "column": "id" },
                "acl": [grant(16400, Some("app"), "USAGE"), grant(10, Some("postgres"), "SELECT")],
            }],
            "roles": [
                { "oid": 16400, "name": "app", "login": true, "inherit": false,
                  "connection_limit": 5, "valid_until": "2030-01-01 00:00:00+00",
                  "bypass_rls": true,
                  "member_of": [
                      { "role": role(16401, "readers"), "grantor": role(10, "postgres"),
                        "admin": true, "inherit": false, "set": true },
                      { "role": role(16399, "neon_superuser"), "grantor": role(10, "postgres"),
                        "admin": false, "inherit": true, "set": true },
                  ],
                  "other_attributes": ["CREATEROLE", "CREATEDB"], "comment": null },
                { "oid": 16401, "name": "Readers", "login": false, "inherit": true,
                  "connection_limit": -1, "valid_until": null, "bypass_rls": false,
                  "member_of": [], "other_attributes": [], "comment": "readers" },
            ],
            "default_privileges": [
                { "oid": 16471, "role": role(10, "postgres"), "schema": null, "object_type": "S",
                  "acl": [grant(16400, Some("app"), "USAGE")] },
                { "oid": 16470, "role": role(10, "postgres"), "schema": "Billing",
                  "object_type": "r", "acl": [grant(16401, Some("Readers"), "SELECT")] },
            ],
            "reported": [
                { "kind": "view", "oid": 16600, "schema": "public", "name": "v", "parent": null,
                  "arguments": null, "definition": " SELECT 1;" },
                { "kind": "aggregate", "oid": 16601, "schema": "public", "name": "agg",
                  "parent": null, "arguments": "integer", "definition": null },
                { "kind": "merge_leftover", "oid": 16602, "schema": "Billing",
                  "name": "chelabase_merge_nn_2_16420", "parent": "Order", "arguments": null,
                  "definition": "CHECK (status IS NOT NULL) NOT VALID" },
                { "kind": "function", "oid": 16603, "schema": "Billing", "name": "f",
                  "parent": null, "arguments": "a integer", "definition": "CREATE FUNCTION …" },
            ],
        })
    }

    #[test]
    fn parse_refuses_pg16() {
        let doc = document(160004).to_string();
        assert_eq!(parse_catalog(&doc), Err(SchemaError::UnsupportedServer(16)));
        // The version is checked before anything else is read.
        assert_eq!(
            parse_catalog(r#"{"server_version_num": 180001}"#),
            Err(SchemaError::UnsupportedServer(18))
        );
    }

    #[test]
    fn parse_sorts_and_normalizes() {
        let m = parse_catalog(&document(170005).to_string()).unwrap();
        assert_eq!(m.model_version, crate::MODEL_VERSION);
        assert_eq!(m.server_major, 17);

        // Schemas sorted by name (byte order), grants PUBLIC first.
        let names: Vec<&str> = m.schemas.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["Billing", "public"]);
        assert_eq!(m.schemas[1].grants[0].grantee, RoleRef::Public);

        let t = &m.tables[0];
        assert!(t.unlogged);
        assert_eq!(t.options, ["autovacuum_enabled=false", "fillfactor=70"]);
        assert_eq!(t.grants[0].grantee, RoleRef::Public);
        assert_eq!(t.grants[0].privilege, "INSERT");

        // Columns by attnum; codes mapped.
        let attnums: Vec<i16> = t.columns.iter().map(|c| c.attnum).collect();
        assert_eq!(attnums, [1, 2, 3]);
        let id = t.columns[0].identity.as_ref().expect("identity");
        assert_eq!(id.generation, IdentityGeneration::Always);
        assert_eq!(id.sequence_oid, Oid(16419));
        assert_eq!(id.sequence_name, "Order_id_seq");
        assert_eq!(
            (id.params.start, id.params.increment, id.params.cache),
            (100, 5, 3)
        );
        assert_eq!(id.grants[0].grantee, RoleRef::Public);
        assert_eq!(t.columns[1].identity, None);
        assert_eq!(t.columns[1].grants[0].privilege, "SELECT");
        assert_eq!(t.columns[2].generated.as_deref(), Some("(id * 2)"));
        assert_eq!(t.columns[2].default, None);

        let kinds: Vec<(&str, ConstraintKind, bool)> = t
            .constraints
            .iter()
            .map(|c| (c.name.as_str(), c.kind, c.validated))
            .collect();
        assert_eq!(
            kinds,
            [
                ("fk", ConstraintKind::ForeignKey, false),
                ("order_pkey", ConstraintKind::PrimaryKey, true)
            ]
        );
        assert_eq!(t.constraints[0].references.as_ref().unwrap().table, "t");

        // The index's keys come out of its definition without its name.
        let ix = &t.indexes[0];
        assert_eq!(ix.name, "ix(\"odd\")");
        assert_eq!(
            ix.keys,
            [
                "lower((\"a,b\")::text) DESC",
                "status text_pattern_ops",
                "(id || ',('::text)"
            ]
        );
        assert_eq!(ix.include, ["id"]);
        assert_eq!(ix.predicate.as_deref(), Some("id > 0"));
        assert_eq!(ix.options, ["deduplicate_items=off", "fillfactor=80"]);
        assert!(!ix.valid);

        let p = &t.policies[0];
        assert_eq!(p.command, PolicyCommand::Update);
        assert!(!p.permissive);
        assert_eq!(p.roles[0], RoleRef::Public);
        assert_eq!(
            p.roles[1],
            RoleRef::Role {
                oid: Oid(16400),
                name: "app".into()
            }
        );

        // Enum labels keep their catalog order.
        let labels: Vec<&str> = m.enums[0].labels.iter().map(|l| l.label.as_str()).collect();
        assert_eq!(labels, ["new", "gone"]);

        let seq = &m.sequences[0];
        assert_eq!(seq.owned_by.as_ref().unwrap().column, "id");
        // Grants by grantee name: "app" before "postgres".
        let privileges: Vec<&str> = seq.grants.iter().map(|g| g.privilege.as_str()).collect();
        assert_eq!(privileges, ["USAGE", "SELECT"]);

        let roles: Vec<&str> = m.roles.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(roles, ["Readers", "app"]);
        let app = &m.roles[1];
        assert!(app.owner_level());
        assert_eq!(app.member_of[0].role.name, "neon_superuser");
        assert_eq!(app.other_attributes, ["CREATEDB", "CREATEROLE"]);

        let dp: Vec<(Option<&str>, DefaultPrivilegeObject)> = m
            .default_privileges
            .iter()
            .map(|d| (d.schema.as_deref(), d.object_type))
            .collect();
        assert_eq!(
            dp,
            [
                (None, DefaultPrivilegeObject::Sequences),
                (Some("Billing"), DefaultPrivilegeObject::Tables)
            ]
        );

        let reported: Vec<(&str, ReportedKind)> = m
            .reported
            .iter()
            .map(|r| (r.name.as_str(), r.kind.clone()))
            .collect();
        assert_eq!(
            reported,
            [
                ("chelabase_merge_nn_2_16420", ReportedKind::MergeLeftover),
                ("f", ReportedKind::Function),
                ("agg", ReportedKind::Other("aggregate".into())),
                ("v", ReportedKind::View),
            ]
        );

        // Already normalized: normalizing again changes nothing.
        let mut again = m.clone();
        again.normalize();
        assert_eq!(again, m);
    }

    #[test]
    fn unknown_attgenerated_is_an_error() {
        let mut doc = document(170005);
        doc["tables"][0]["columns"][0]["generated"] = json!("v");
        assert_eq!(
            parse_catalog(&doc.to_string()),
            Err(SchemaError::UnknownCatalogValue {
                what: "attgenerated".into(),
                value: "v".into()
            })
        );
    }

    #[test]
    fn unknown_codes_are_errors() {
        for (pointer, what, value) in [
            ("/tables/0/constraints/0/contype", "contype", "n"),
            ("/tables/0/policies/0/command", "polcmd", "z"),
            ("/tables/0/persistence", "relpersistence", "t"),
            ("/tables/0/columns/0/identity", "attidentity", "x"),
            ("/default_privileges/0/object_type", "defaclobjtype", "L"),
        ] {
            let mut doc = document(170005);
            *doc.pointer_mut(pointer).unwrap() = json!(value);
            assert_eq!(
                parse_catalog(&doc.to_string()),
                Err(SchemaError::UnknownCatalogValue {
                    what: what.into(),
                    value: value.into()
                }),
                "{pointer}"
            );
        }
    }

    #[test]
    fn unreadable_documents_are_parse_errors() {
        assert!(matches!(
            parse_catalog("not json"),
            Err(SchemaError::Parse(_))
        ));
        let mut doc = document(170005);
        doc["tables"][0]["indexes"][0]["key_count"] = json!(2);
        assert!(matches!(
            parse_catalog(&doc.to_string()),
            Err(SchemaError::Parse(_))
        ));
    }

    /// The script is valid SQL to Postgres's own parser (a live read checks the rest).
    #[test]
    fn catalog_sql_parses() {
        let parsed = pg_query::parse(CATALOG_SQL).expect("CATALOG_SQL parses");
        // BEGIN, 3 × SET, DO, SELECT, COMMIT.
        assert_eq!(parsed.protobuf.stmts.len(), 7);
    }

    /// P7: the default-ACL read keeps the data API's per-schema rows the model excludes, and is
    /// otherwise the catalog read.
    #[test]
    fn default_acl_sql_keeps_the_data_api_rows() {
        let sql = default_acl_sql();
        assert_ne!(sql, CATALOG_SQL, "the filter was found and removed");
        assert!(!sql.contains("IN ('anon', 'authenticated')"), "{sql}");
        assert!(CATALOG_SQL.contains("IN ('anon', 'authenticated')"));
        let parsed = pg_query::parse(&sql).expect("the default-ACL read parses");
        assert_eq!(parsed.protobuf.stmts.len(), 7);
        assert!(sql.starts_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;\n"));
    }

    /// T14 M5: the data API's per-schema filter only hides the owner's rows, in both reads.
    #[test]
    fn data_api_filter_is_only_for_the_owners_rows() {
        assert!(DATA_API_DEFAULT_ACL_FILTER.contains("dr.rolname = 'postgres'"));
        assert!(CATALOG_SQL.contains(DATA_API_DEFAULT_ACL_FILTER));
        // E16's row is excluded from both reads (functions aren't simulated, P7).
        assert!(default_acl_sql().contains(exclusions::DATA_API_FUNCTIONS_DEFAULT_ACL));
    }

    /// T14 minor 14: policies carry their comment.
    #[test]
    fn policy_comments_are_read() {
        assert!(CATALOG_SQL.contains("'comment', obj_description(p.oid, 'pg_policy')"));
    }

    /// T14 M6: a table that INHERITS from another is its own reported kind.
    #[test]
    fn inherited_tables_are_a_reported_kind() {
        assert_eq!(
            reported_kind("inherited_table".to_string()),
            ReportedKind::InheritedTable
        );
    }

    #[test]
    fn index_keys_skip_names_and_split_at_top_level_commas() {
        assert_eq!(
            index_keys("CREATE INDEX \"a (b\" ON public.\"t,(\" USING gin (x, (f(y, 'a,)')))"),
            Some(vec!["x".to_string(), "(f(y, 'a,)'))".to_string()])
        );
        assert_eq!(index_keys("CREATE INDEX i ON t USING btree"), None);
    }

    /// The exclusion lists the SQL hard-codes are the ones in [`exclusions`].
    #[test]
    fn catalog_sql_names_every_exclusion() {
        let quoted = |s: &str| format!("'{s}'");
        let mut names: Vec<String> = Vec::new();
        names.extend(exclusions::EXCLUDED_SCHEMAS.iter().map(|s| quoted(s)));
        names.extend(exclusions::RESERVED_ROLES.iter().map(|s| quoted(s)));
        names.extend(exclusions::USERS_MANAGED_COLUMNS.iter().map(|s| quoted(s)));
        names.extend(exclusions::USERS_GUARD_TRIGGERS.iter().map(|s| quoted(s)));
        names.extend(exclusions::USERS_POLICIES.iter().map(|s| quoted(s)));
        names.extend(exclusions::USERS_GRANTEES.iter().map(|s| quoted(s)));
        names.extend(
            exclusions::DATA_API_DEFAULT_GRANTEES
                .iter()
                .map(|s| quoted(s)),
        );
        names.push(quoted(exclusions::USERS_EMAIL_INDEX));
        names.push(quoted(exclusions::APP_USERS_CONFIG_TABLE));
        for prefix in exclusions::EXCLUDED_SCHEMA_PREFIXES {
            names.push(format!("'{}%'", prefix.replace('_', "\\_")));
        }
        names.push(format!(
            "'{}%'",
            exclusions::RESERVED_ROLE_PREFIX.replace('_', "\\_")
        ));
        names.push(format!(
            "lower(r.rolname) LIKE '{}%'",
            exclusions::CHELABASE_ROLE_PREFIX.replace('_', "\\_")
        ));
        // Both the `reserved` and the `hidden` CTE carry both prefixes (checked per CTE below).
        let cte = |start: &str| {
            let from = CATALOG_SQL.find(start).unwrap_or_else(|| panic!("no {start}"));
            let len = CATALOG_SQL[from..].find("\n),").unwrap();
            &CATALOG_SQL[from..from + len]
        };
        for slice in [cte("reserved AS ("), cte("hidden AS (")] {
            for prefix in [
                exclusions::CHELABASE_ROLE_PREFIX,
                exclusions::MEMBER_ROLE_PREFIX,
            ] {
                let like = format!("lower(r.rolname) LIKE '{}%'", prefix.replace('_', "\\_"));
                assert!(slice.contains(&like), "{like} missing in {slice}");
            }
        }
        // Memberships of or by a hidden role are not read.
        assert!(CATALOG_SQL.contains("AND mr.oid NOT IN (SELECT oid FROM hidden)"));
        assert!(CATALOG_SQL.contains("AND mg.oid NOT IN (SELECT oid FROM hidden)"));
        // E16, and M5: the data API's per-schema filter only for the owner's rows.
        names.push(exclusions::DATA_API_FUNCTIONS_DEFAULT_ACL.to_string());
        names.push(format!(
            "dr.rolname = '{}'",
            exclusions::DATA_API_DEFAULT_ACL_ROLE
        ));
        // M6: tables that INHERIT are reported, not read as tables.
        names.push("'inherited_table'".to_string());
        names.push(format!(
            "'{}%'",
            exclusions::MERGE_NOT_NULL_CHECK_PREFIX.replace('_', "\\_")
        ));
        names.push(format!(
            "'{}%'",
            exclusions::MERGE_INDEX_PREFIX.replace('_', "\\_")
        ));
        for name in names {
            assert!(CATALOG_SQL.contains(&name), "CATALOG_SQL lacks {name}");
        }
        assert!(CATALOG_SQL.starts_with(
            "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;\nSET LOCAL search_path = pg_catalog;\n"
        ));
        assert!(CATALOG_SQL.trim_end().ends_with("COMMIT;"));
        // Every aclexplode (one per ACL read) has its grantee filter against the hidden roles.
        assert_eq!(
            CATALOG_SQL.matches("aclexplode(").count(),
            CATALOG_SQL.matches("grantee NOT IN (SELECT oid FROM hidden)").count()
        );
        assert!(
            !CATALOG_SQL.contains("pg_authid "),
            "roles come from pg_roles"
        );
    }

    #[test]
    fn table_sql_names_its_target_safely() {
        let sql = table_sql("we'ird", "Na\"me");
        assert!(sql.contains("'we''ird'"), "{sql}");
        assert!(sql.contains("'Na\"me'"), "{sql}");
        assert!(!sql.contains("chelabase.app_users_table") || sql.contains("has_table_privilege"), "the owner can't read the users-table pointer: {sql}");
    }

    #[test]
    fn a_missing_table_parses_to_none() {
        assert_eq!(parse_table(r#"{"server_version_num": 170004, "table": null}"#).unwrap(), None);
        assert!(parse_table(r#"{"server_version_num": 160000, "table": null}"#).is_err(), "only 17");
    }

    #[test]
    fn objects_and_types_parse_sorted() {
        let objects = parse_objects(r#"{"server_version_num": 170004, "objects": [
            {"schema":"public","name":"z","kind":"view","oid":2,"owner":"o","comment":null,"rls_enabled":false,"rls_forced":false,"data_api_exposed":false,"row_estimate":null,"total_bytes":0,"editable":false},
            {"schema":"public","name":"a","kind":"table","oid":1,"owner":"o","comment":"c","rls_enabled":true,"rls_forced":false,"data_api_exposed":true,"row_estimate":10,"total_bytes":8192,"editable":true}
        ]}"#)
        .unwrap();
        assert_eq!(objects.iter().map(|o| o.name.as_str()).collect::<Vec<_>>(), ["a", "z"]);
        assert_eq!(objects[1].kind, chelabase_types::table_api::ObjectKind::View);
        let types = parse_types(r#"{"server_version_num": 170004, "types": [
            {"oid":20,"schema":"pg_catalog","name":"int8","kind":"base","category":"N","preferred":false,"takes_modifiers":false,"array_oid":1016,"labels":[],"base_type_oid":null},
            {"oid":9,"schema":"app","name":"mood","kind":"enum","category":"E","preferred":false,"takes_modifiers":false,"array_oid":10,"labels":["sad","ok"],"base_type_oid":null}
        ]}"#)
        .unwrap();
        assert_eq!(types.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["mood", "int8"]);
    }

    /// T4: each scoped read is one transaction of the catalog read's shape (BEGIN, 3 × SET, DO,
    /// SELECT, COMMIT), valid SQL to Postgres's own parser.
    #[test]
    fn scoped_reads_parse() {
        for (what, sql) in [
            ("table", table_sql("public", "t")),
            ("objects", objects_sql()),
            ("types", types_sql()),
        ] {
            let parsed = pg_query::parse(&sql).unwrap_or_else(|e| panic!("{what}: {e}\n{sql}"));
            assert_eq!(parsed.protobuf.stmts.len(), 7, "{what}");
            assert!(
                sql.starts_with("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;\nSET LOCAL search_path = pg_catalog;\n"),
                "{what}"
            );
            assert!(sql.trim_end().ends_with("COMMIT;"), "{what}");
        }
    }

    /// T4: the one-table read and the whole catalog read share their table rules and per-table
    /// object, so one table reads as the whole catalog reads it.
    #[test]
    fn table_sql_shares_the_catalog_reads_table_fragment() {
        let sql = table_sql("public", "t");
        for part in [HEAD_SQL, TABS_SQL, TABLE_OBJECT_SQL] {
            assert!(CATALOG_SQL.contains(part));
            assert!(sql.contains(part));
        }
        assert!(sql.contains("AND n.nspname = 'public' AND c.relname = 't'\n),"), "{sql}");
        // Every exclusion in the table object (users-table columns, policies, grantees, the
        // email index, the merge's own constraints and indexes) is in the shared fragment.
        for name in exclusions::USERS_MANAGED_COLUMNS
            .iter()
            .chain(exclusions::USERS_POLICIES.iter())
            .chain(exclusions::USERS_GRANTEES.iter())
            .chain([exclusions::USERS_EMAIL_INDEX].iter())
        {
            assert!(TABLE_OBJECT_SQL.contains(&format!("'{name}'")), "{name}");
        }
        // The object list's `editable` is exactly "read as a table".
        assert!(objects_sql().contains(TABS_SQL));
        assert!(OBJECTS_SQL.contains("'editable', c.oid IN (SELECT oid FROM tabs)"));
    }

    /// T4: the owner can't read the users-table pointer: the head reads it only behind the
    /// privilege checks, and finds its table through `pg_catalog` (`to_regclass` on a schema
    /// without USAGE raises), falling back to Chelabase's guard trigger.
    #[test]
    fn the_users_table_pointer_is_read_only_when_permitted() {
        let guard = HEAD_SQL
            .find("IF has_schema_privilege('chelabase', 'USAGE') AND has_table_privilege(pointer, 'SELECT') THEN")
            .expect("guard");
        let read = HEAD_SQL.find("FROM chelabase.app_users_table u").expect("read");
        let fallback = HEAD_SQL.find("ELSE").expect("fallback");
        assert!(guard < read && read < fallback);
        assert!(!HEAD_SQL.contains("to_regclass"), "{HEAD_SQL}");
        assert!(HEAD_SQL.contains("tg.tgname = 'chelabase_users_guard'"));
        assert!(HEAD_SQL.contains("HAVING count(DISTINCT tg.tgrelid) = 1"));
    }

    /// No Postgres name holds a NUL: such a name matches nothing instead of cutting the query
    /// text short.
    #[test]
    fn a_name_with_a_nul_matches_nothing() {
        let sql = table_sql("pub\0lic", "t");
        assert!(sql.contains("AND n.nspname = NULL AND c.relname = 't'"), "{sql}");
        assert!(!sql.contains('\0'));
        let sql = table_sql("a\\b", "t");
        assert!(sql.contains("n.nspname = E'a\\\\b'"), "{sql}");
    }

    #[test]
    fn scoped_documents_refuse_other_servers() {
        assert_eq!(
            parse_objects(r#"{"server_version_num": 180001, "objects": []}"#),
            Err(SchemaError::UnsupportedServer(18))
        );
        assert_eq!(
            parse_types(r#"{"server_version_num": 160004, "types": []}"#),
            Err(SchemaError::UnsupportedServer(16))
        );
        assert_eq!(
            parse_table(r#"{"server_version_num": 160004}"#),
            Err(SchemaError::UnsupportedServer(16))
        );
        assert!(matches!(
            parse_objects(r#"{"server_version_num": 170004, "objects": [{"schema": "x"}]}"#),
            Err(SchemaError::Parse(_))
        ));
    }

    /// One table parses (and normalizes) as the whole catalog's table does.
    #[test]
    fn one_table_parses_as_in_the_whole_catalog() {
        let doc = document(170005);
        let whole = parse_catalog(&doc.to_string()).unwrap();
        let one = json!({ "server_version_num": 170005, "table": doc["tables"][0] });
        assert_eq!(parse_table(&one.to_string()).unwrap().as_ref(), whole.tables.first());
    }

    // --- gc: `GET …/tables/{schema}/{table}` answers, read-only, for every kind the object
    // list shows (`relation_sql` / `parse_relation`, `model::Relation`). `table_sql` and
    // `parse_table` stay the ordinary-table read that the row and DDL paths use.

    use chelabase_types::table_api::ObjectKind;

    /// Every [`ObjectKind`]; the match fails to compile when a kind is added, so this list
    /// (and the tests over it) can't fall behind the object list.
    fn every_object_kind() -> [ObjectKind; 5] {
        let all = [
            ObjectKind::Table,
            ObjectKind::PartitionedTable,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::ForeignTable,
        ];
        for k in all {
            match k {
                ObjectKind::Table
                | ObjectKind::PartitionedTable
                | ObjectKind::View
                | ObjectKind::MaterializedView
                | ObjectKind::ForeignTable => {}
            }
        }
        all
    }

    fn kind_text(kind: ObjectKind) -> String {
        serde_json::to_value(kind).unwrap().as_str().unwrap().to_string()
    }

    /// [`relation_sql`]'s document: `{"server_version_num", "kind", "table"}`, the object in
    /// [`table_sql`]'s shape. Columns out of attnum order; every list the non-table kinds drop is
    /// filled, so the parser is seen to drop them.
    fn relation_document(kind: Option<&str>) -> Value {
        let mut placed_at = column(1, "placed_at");
        placed_at["data_type"] = json!("timestamp with time zone");
        placed_at["type_oid"] = json!(1184);
        let mut id = column(2, "id");
        id["data_type"] = json!("uuid");
        id["type_oid"] = json!(2950);
        id["not_null"] = json!(true);
        let mut amount = column(3, "amount");
        amount["data_type"] = json!("numeric(12,2)");
        amount["type_oid"] = json!(1700);
        let note = column(4, "note");
        let mut object = document(170005)["tables"][0].clone();
        object["name"] = json!("orders_v");
        object["schema"] = json!("public");
        object["columns"] = json!([amount, note, id, placed_at]);
        json!({ "server_version_num": 170005, "kind": kind, "table": object })
    }

    #[test]
    fn parse_relation_reads_every_object_kind() {
        for kind in every_object_kind() {
            let doc = relation_document(Some(&kind_text(kind)));
            let r = parse_relation(&doc.to_string())
                .unwrap_or_else(|e| panic!("{kind:?}: {e:?}"))
                .unwrap_or_else(|| panic!("{kind:?}: none"));
            assert_eq!(r.kind, kind);
            assert_eq!((r.table.schema.as_str(), r.table.name.as_str()), ("public", "orders_v"));
            // Columns by attnum, with format_type's text and attnotnull.
            let columns: Vec<(i16, &str, &str, bool)> = r
                .table
                .columns
                .iter()
                .map(|c| (c.attnum, c.name.as_str(), c.data_type.as_str(), c.not_null))
                .collect();
            assert_eq!(
                columns,
                [
                    (1, "placed_at", "timestamp with time zone", false),
                    (2, "id", "uuid", true),
                    (3, "amount", "numeric(12,2)", false),
                    (4, "note", "integer", false),
                ],
                "{kind:?}"
            );
        }
    }

    /// An ordinary table reads exactly as [`parse_table`] reads it: constraints, indexes,
    /// policies, grants and options kept.
    #[test]
    fn parse_relation_reads_a_table_as_parse_table_does() {
        let doc = relation_document(Some("table"));
        let r = parse_relation(&doc.to_string()).unwrap().expect("a table");
        let one = json!({ "server_version_num": 170005, "table": doc["table"] });
        assert_eq!(Some(r.table), parse_table(&one.to_string()).unwrap());
        assert_eq!(r.kind, ObjectKind::Table);
    }

    /// A view, materialized view, foreign table or partitioned table is read-only: its columns
    /// only. Constraints, indexes, policies, grants and options are empty lists (required fields
    /// of the generated `Table` type), whatever the document carried.
    #[test]
    fn parse_relation_keeps_only_columns_for_other_kinds() {
        for kind in every_object_kind().into_iter().filter(|k| *k != ObjectKind::Table) {
            let doc = relation_document(Some(&kind_text(kind)));
            let r = parse_relation(&doc.to_string()).unwrap().unwrap();
            let t = &r.table;
            assert!(t.constraints.is_empty(), "{kind:?}: {:?}", t.constraints);
            assert!(t.indexes.is_empty(), "{kind:?}: {:?}", t.indexes);
            assert!(t.policies.is_empty(), "{kind:?}: {:?}", t.policies);
            assert!(t.grants.is_empty(), "{kind:?}: {:?}", t.grants);
            assert!(t.options.is_empty(), "{kind:?}: {:?}", t.options);
            assert_eq!(t.columns.len(), 4, "{kind:?}");
        }
    }

    #[test]
    fn parse_relation_of_nothing_is_none() {
        assert_eq!(
            parse_relation(r#"{"server_version_num": 170004, "kind": null, "table": null}"#).unwrap(),
            None
        );
        assert_eq!(
            parse_relation(r#"{"server_version_num": 160004, "kind": null, "table": null}"#),
            Err(SchemaError::UnsupportedServer(16))
        );
    }

    /// A relation of a kind the endpoint doesn't serve (sequence, index, composite type, toast,
    /// or no kind at all) is never parsed into an answer.
    #[test]
    fn parse_relation_refuses_other_kinds() {
        for kind in [Some("sequence"), Some("index"), Some("composite_type"), Some("toast"), Some("S"), None] {
            let doc = relation_document(kind);
            assert!(parse_relation(&doc.to_string()).is_err(), "{kind:?}");
        }
    }

    /// The response is the `Table` object with `kind` beside its fields (flattened), so a
    /// client reading the generated `Table` type keeps working.
    #[test]
    fn a_relation_serializes_as_a_table_with_its_kind() {
        let r = parse_relation(&relation_document(Some("view")).to_string()).unwrap().unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["kind"], "view", "{v}");
        assert_eq!(v["name"], "orders_v", "{v}");
        assert!(v.get("table").is_none(), "flattened: {v}");
        for list in ["constraints", "indexes", "policies", "grants", "options"] {
            assert_eq!(v[list], json!([]), "{list} in {v}");
        }
        assert_eq!(v["columns"][2]["data_type"], "numeric(12,2)", "{v}");
        let mut table = serde_json::to_value(&r.table).unwrap();
        table.as_object_mut().unwrap().insert("kind".into(), json!("view"));
        assert_eq!(v, table, "exactly the table's fields plus kind");
    }

    #[test]
    fn relation_sql_is_one_read_only_transaction() {
        let sql = relation_sql("public", "t");
        let parsed = pg_query::parse(&sql).unwrap_or_else(|e| panic!("{e}\n{sql}"));
        assert_eq!(parsed.protobuf.stmts.len(), 7, "BEGIN, 3 × SET, DO, SELECT, COMMIT");
        assert!(sql.starts_with(
            "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;\nSET LOCAL search_path = pg_catalog;\n"
        ));
        assert!(sql.trim_end().ends_with("COMMIT;"));
    }

    #[test]
    fn relation_sql_names_its_target_safely() {
        let sql = relation_sql("we'ird", "Na\"me");
        assert!(sql.contains("'we''ird'"), "{sql}");
        assert!(sql.contains("'Na\"me'"), "{sql}");
        let sql = relation_sql("pub\0lic", "t");
        assert!(!sql.contains('\0'));
        assert!(!sql.contains("'pub"), "a NUL name matches nothing: {sql}");
        assert!(sql.contains("= NULL"), "{sql}");
    }

    /// An ordinary table reads as [`table_sql`] reads it (the same `tabs` rules and object);
    /// the other kinds come from the object list's schemas (`ns`) without extension members, as
    /// the object list finds them.
    #[test]
    fn relation_sql_reads_tables_and_scopes_as_the_object_list() {
        let sql = relation_sql("public", "t");
        assert!(sql.contains(HEAD_SQL));
        assert!(sql.contains(TABS_SQL));
        assert!(sql.contains(TABLE_OBJECT_SQL));
        let ext = "NOT EXISTS (SELECT FROM ext e WHERE e.classid = 'pg_class'::regclass AND e.objid = c.oid)";
        assert!(OBJECTS_SQL.contains(ext));
        assert!(sql.matches(ext).count() >= 2, "tables and the other kinds both leave out extension members: {sql}");
        let scoped = "JOIN ns n ON n.oid = c.relnamespace";
        assert!(TABS_SQL.contains(scoped) && OBJECTS_SQL.contains(scoped));
        assert!(sql.matches(scoped).count() >= 2, "the other kinds are read from `ns` too: {sql}");
    }

    /// Nothing reads a view's or rule's definition, or a foreign table's, server's or user
    /// mapping's options (file names, hosts, credentials).
    #[test]
    fn relation_sql_never_reads_definitions_or_foreign_options() {
        let sql = relation_sql("public", "t");
        for leak in [
            "pg_get_viewdef",
            "pg_get_ruledef",
            "pg_rewrite",
            "ev_action",
            "pg_views",
            "pg_matviews",
            "pg_foreign_table",
            "ftoptions",
            "pg_foreign_server",
            "srvoptions",
            "pg_user_mapping",
            "umoptions",
            "attfdwoptions",
        ] {
            assert!(!sql.contains(leak), "{leak} in {sql}");
        }
    }

    // --- gc (coder): edges beyond the RED set.

    /// Every relation the object list lists answers: a table that inherits from another (which
    /// `tabs` leaves out) is read with the full table object as kind `table`, while `table_sql`
    /// (the row and DDL paths) still leaves it out.
    #[test]
    fn relation_sql_reads_an_inheriting_table_as_a_table() {
        let sql = relation_sql("public", "child");
        assert!(sql.contains("AND EXISTS (SELECT FROM pg_inherits ih WHERE ih.inhrelid = c.oid)"), "{sql}");
        assert!(sql.contains("FROM (SELECT * FROM tabs UNION ALL SELECT * FROM inherits) t"), "{sql}");
        assert!(sql.contains("(SELECT 'table'::text FROM tables_j)"), "{sql}");
        assert!(!table_sql("public", "child").contains("inherits AS"));
        // The object list's kinds and partition rule, and nothing else.
        assert!(OBJECTS_SQL.contains("c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition"));
        assert!(sql.contains("c.relkind IN ('p', 'v', 'm', 'f') AND NOT c.relispartition"), "{sql}");
        assert!(sql.contains("c.relkind = 'r' AND NOT c.relispartition"), "{sql}");
        // Each of the three sources names the target.
        assert_eq!(sql.matches("AND n.nspname = 'public' AND c.relname = 'child'").count(), 3, "{sql}");
    }

    /// A kind without its object, or an object without its kind, is a malformed document, never
    /// an answer or a 404.
    #[test]
    fn parse_relation_refuses_a_half_document() {
        let doc = relation_document(Some("view"));
        let no_object = json!({ "server_version_num": 170005, "kind": "view", "table": null });
        assert!(matches!(parse_relation(&no_object.to_string()), Err(SchemaError::Parse(_))));
        let no_kind = json!({ "server_version_num": 170005, "kind": null, "table": doc["table"] });
        assert!(matches!(parse_relation(&no_kind.to_string()), Err(SchemaError::Parse(_))));
        // A missing `kind` key reads as null.
        let absent = json!({ "server_version_num": 170005, "table": null });
        assert_eq!(parse_relation(&absent.to_string()).unwrap(), None);
    }

    /// The read-only object is the table object's shape with every list but the columns fixed
    /// empty: no ACL, option, constraint, index or policy is read for those kinds.
    #[test]
    fn relation_object_sql_reads_columns_only() {
        for empty in ["'options', '[]'::jsonb", "'acl', '[]'::jsonb", "'constraints', '[]'::jsonb", "'indexes', '[]'::jsonb", "'policies', '[]'::jsonb"] {
            assert!(RELATION_OBJECT_SQL.contains(empty), "{empty}");
        }
        for table_only in ["aclexplode", "pg_constraint", "pg_index", "pg_policy", "reloptions"] {
            assert!(!RELATION_OBJECT_SQL.contains(table_only), "{table_only}");
        }
        assert!(RELATION_OBJECT_SQL.contains("format_type(a.atttypid, a.atttypmod)"));
        assert!(RELATION_OBJECT_SQL.contains("ORDER BY a.attnum"));
        assert!(RELATION_OBJECT_SQL.contains("NOT a.attisdropped"));
    }

    /// The type catalog leaves out pseudo-types, arrays and relations' row types.
    #[test]
    fn types_sql_names_its_exclusions() {
        assert!(TYPES_SQL.contains("ty.typtype IN ('b', 'e', 'd', 'c', 'r', 'm')"));
        assert!(TYPES_SQL.contains("NOT EXISTS (SELECT FROM pg_type el WHERE el.typarray = ty.oid)"));
        assert!(TYPES_SQL.contains("WHERE rc.oid = ty.typrelid) = 'c')"));
        assert!(TYPES_SQL.contains("n.nspname = 'pg_catalog' OR n.oid IN (SELECT oid FROM ns)"));
        // `ns` leaves out information_schema and Chelabase's schemas.
        assert!(HEAD_SQL.contains("'information_schema'"));
    }
}
