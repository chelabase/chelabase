//! The schema cache: what a branch's database holds at one schema version, and what one
//! role can reach of it.
//!
//! [`INTROSPECT_SQL`] reads every table, view, column, type, enum and function outside
//! [`RESERVED_SCHEMAS`] in one round trip, as JSON. [`VISIBILITY_SQL`] asks, for one role
//! named explicitly, which schemas it has `USAGE` on, which tables it holds a privilege on
//! (table or column level) and which functions it may execute. A [`RoleView`] built from
//! the two is the planner's [`Catalog`]: anything the role can't reach isn't in it, so it
//! answers exactly like a name that doesn't exist.
//!
//! Functions are callable only when every input argument is named (calls use named
//! notation), none is variadic or of a pseudo-type, they return a real type, `void` or
//! `record`, and no extension owns them (an extension's functions are Postgres machinery,
//! never part of the API).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use chelabase_types::data_api_schema::{
    AppUsersTable, ArgDesc, ColumnDesc, DataApiSchema, FunctionDesc, Privilege, ReturnDesc, SchemaEntry, TableDesc,
    TableKind,
};
use serde::Deserialize;
use serde_json::Value;
use tokio_postgres::Client;
use tokio_postgres::types::Type;

use crate::query::Name;
use crate::sql::{Catalog, ColumnInfo, FnLookupError, FunctionInfo, TableInfo, quote_ident};

/// Schemas never exposed, whatever the grants: Chelabase's own machinery, Postgres's and
/// Neon's own. `pg_temp_*` and `pg_toast_temp_*` are hidden too ([`is_reserved`]).
pub const RESERVED_SCHEMAS: &[&str] = &[
    "chelabase",
    "pg_catalog",
    "information_schema",
    "pg_toast",
    "neon",
    "neon_migration",
];

/// Each overload of one function, with the names of its arguments that have no default.
type Overloads = Vec<(FunctionInfo, Vec<String>)>;
/// A schema's tables and its functions' overloads, for the JSON.
type SchemaJson = (
    BTreeMap<String, TableDesc>,
    BTreeMap<String, Vec<FunctionDesc>>,
);

/// Roles cached per schema version; past this many, the oldest are dropped.
const ROLE_VIEWS: u64 = 64;

pub fn is_reserved(schema: &str) -> bool {
    RESERVED_SCHEMAS.contains(&schema)
        || schema.starts_with("pg_temp_")
        || schema.starts_with("pg_toast_temp_")
}

/// `$1` = [`RESERVED_SCHEMAS`] (`text[]`). One row, one `text` column: the JSON [`Raw`] reads (oids
/// as `int8`, since `to_json` writes an `oid` as a string).
pub const INTROSPECT_SQL: &str = "SELECT pg_catalog.json_build_object(
  'namespaces', (SELECT coalesce(pg_catalog.json_agg(pg_catalog.json_build_array(n.oid::int8, n.nspname)), '[]')
    FROM pg_catalog.pg_namespace n WHERE NOT (n.nspname = ANY ($1) OR n.nspname ~ '^pg_(toast_)?temp_')),
  'types', (SELECT coalesce(pg_catalog.json_agg(pg_catalog.json_build_array(t.oid::int8, n.nspname, t.typname, t.typtype,
      t.typelem::int8, t.typbasetype::int8, t.typcategory, t.typlen, pg_catalog.format_type(t.oid, NULL))), '[]')
    FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace),
  'enums', (SELECT coalesce(pg_catalog.json_agg(pg_catalog.json_build_array(e.enumtypid::int8, e.enumlabel)
      ORDER BY e.enumtypid, e.enumsortorder), '[]') FROM pg_catalog.pg_enum e),
  'relations', (SELECT coalesce(pg_catalog.json_agg(pg_catalog.json_build_array(c.oid::int8, n.nspname, c.relname, c.relkind,
      c.relrowsecurity,
      c.relkind = 'v' AND coalesce((SELECT pg_catalog.bool_or(pg_catalog.substr(o, 18)::boolean)
        FROM pg_catalog.unnest(c.reloptions) o WHERE o LIKE 'security_invoker=%'), false),
      (SELECT coalesce(pg_catalog.json_agg(pg_catalog.json_build_array(a.attname, a.atttypid::int8,
          pg_catalog.format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
          a.atthasdef OR a.attidentity <> '', a.attgenerated <> '' OR a.attidentity = 'a') ORDER BY a.attnum), '[]')
        FROM pg_catalog.pg_attribute a WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped),
      (SELECT coalesce(pg_catalog.json_agg(a.attname ORDER BY k.ord), '[]')
        FROM pg_catalog.pg_constraint con
        CROSS JOIN LATERAL pg_catalog.unnest(con.conkey) WITH ORDINALITY k(attnum, ord)
        JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum
        WHERE con.conrelid = c.oid AND con.contype = 'p'))), '[]')
    FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT (n.nspname = ANY ($1) OR n.nspname ~ '^pg_(toast_)?temp_')),
  'functions', (SELECT coalesce(pg_catalog.json_agg(pg_catalog.json_build_array(p.oid::int8, n.nspname, p.proname, p.prokind,
      p.proretset, p.prorettype::int8, p.proargtypes::oid[]::int8[], p.proallargtypes::int8[], p.proargmodes::text[], p.proargnames,
      p.pronargdefaults, p.provariadic <> 0,
      EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass
        AND d.objid = p.oid AND d.deptype = 'e'))), '[]')
    FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
    WHERE NOT (n.nspname = ANY ($1) OR n.nspname ~ '^pg_(toast_)?temp_'))
)::text";

/// `$1` = the role (`text`), `$2` = [`RESERVED_SCHEMAS`] (`text[]`). One row, one `text` column: the JSON
/// [`Visibility`] reads.
pub const VISIBILITY_SQL: &str = "SELECT pg_catalog.json_build_object(
  'schemas', (SELECT coalesce(pg_catalog.json_agg(n.oid::int8), '[]') FROM pg_catalog.pg_namespace n
    WHERE NOT (n.nspname = ANY ($2) OR n.nspname ~ '^pg_(toast_)?temp_')
      AND pg_catalog.has_schema_privilege($1::name, n.oid, 'USAGE')),
  'tables', (SELECT coalesce(pg_catalog.json_agg(pg_catalog.json_build_array(c.oid::int8,
      pg_catalog.has_any_column_privilege($1::name, c.oid, 'SELECT'),
      pg_catalog.has_any_column_privilege($1::name, c.oid, 'INSERT'),
      pg_catalog.has_any_column_privilege($1::name, c.oid, 'UPDATE'),
      pg_catalog.has_table_privilege($1::name, c.oid, 'DELETE'))), '[]')
    FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT (n.nspname = ANY ($2) OR n.nspname ~ '^pg_(toast_)?temp_')
      AND pg_catalog.has_schema_privilege($1::name, n.oid, 'USAGE')),
  'functions', (SELECT coalesce(pg_catalog.json_agg(p.oid::int8), '[]')
    FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
    WHERE NOT (n.nspname = ANY ($2) OR n.nspname ~ '^pg_(toast_)?temp_')
      AND pg_catalog.has_schema_privilege($1::name, n.oid, 'USAGE')
      AND pg_catalog.has_function_privilege($1::name, p.oid, 'EXECUTE'))
)::text";

#[derive(Debug, thiserror::Error)]
pub enum IntrospectError {
    #[error(transparent)]
    Db(#[from] tokio_postgres::Error),
    #[error("reading the catalog's JSON: {0}")]
    Decode(#[from] serde_json::Error),
}

/// `(oid, schema, name, typtype, typelem, typbasetype, typcategory, typlen, format_type)`.
type RawType = (u32, String, String, String, u32, u32, String, i16, String);
/// `(name, type oid, format_type with typmod, nullable, has_default, generated)`.
type RawColumn = (String, u32, String, bool, bool, bool);
/// `(oid, schema, name, relkind, rls, security_invoker, columns, primary key columns)`.
/// `security_invoker` is true only for a view whose options set it (any spelling Postgres
/// reads as true: the value is cast to `boolean`). The primary key's columns come in the
/// key's order.
type RawRelation = (
    u32,
    String,
    String,
    String,
    bool,
    bool,
    Vec<RawColumn>,
    Vec<String>,
);
/// `(oid, schema, name, prokind, retset, rettype, argtypes, allargtypes, modes, names,
/// nargdefaults, variadic, owned by an extension)`.
type RawFunction = (
    u32,
    String,
    String,
    String,
    bool,
    u32,
    Vec<u32>,
    Option<Vec<u32>>,
    Option<Vec<String>>,
    Option<Vec<String>>,
    i16,
    bool,
    bool,
);

/// [`INTROSPECT_SQL`]'s JSON.
#[derive(Default, Deserialize)]
pub struct Raw {
    pub namespaces: Vec<(u32, String)>,
    pub types: Vec<RawType>,
    pub enums: Vec<(u32, String)>,
    pub relations: Vec<RawRelation>,
    pub functions: Vec<RawFunction>,
    /// `(schema, table)` of the app users table; not in [`INTROSPECT_SQL`]: [`introspect`]
    /// fills it from [`APP_USERS_TABLE_SQL`].
    #[serde(default)]
    pub app_users: Option<(String, String)>,
}

/// A type as the API describes it: the base type's name (domains resolved, the element
/// type for arrays), whether it's an array, and the enum it is, as `schema.name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeDesc {
    pub name: String,
    pub array: bool,
    pub enum_ref: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub info: ColumnInfo,
    pub ty: TypeDesc,
    pub nullable: bool,
    pub has_default: bool,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    /// `table` (`r`, `p`), `view`, `materialized_view` or `foreign_table`.
    pub kind: TableKind,
    pub rls: bool,
    /// A view that runs with the caller's privileges (and so under its RLS).
    pub security_invoker: bool,
    pub columns: Vec<Column>,
    /// The primary key's columns, in the key's order; empty without one.
    pub primary_key: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Arg {
    pub name: String,
    pub ty: TypeDesc,
    pub has_default: bool,
}

/// A callable function.
#[derive(Debug, Clone)]
pub struct Function {
    pub oid: u32,
    pub info: FunctionInfo,
    pub args: Vec<Arg>,
    pub returns: TypeDesc,
}

impl Function {
    fn required(&self) -> impl Iterator<Item = &str> {
        self.args
            .iter()
            .filter(|a| !a.has_default)
            .map(|a| a.name.as_str())
    }
}

/// What one role holds on a table (column-level grants count for select, insert, update).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
pub struct Privileges {
    pub select: bool,
    pub insert: bool,
    pub update: bool,
    pub delete: bool,
}

impl Privileges {
    pub fn any(&self) -> bool {
        self.select || self.insert || self.update || self.delete
    }

    pub fn held(&self) -> Vec<Privilege> {
        [
            (self.select, Privilege::Select),
            (self.insert, Privilege::Insert),
            (self.update, Privilege::Update),
            (self.delete, Privilege::Delete),
        ]
        .into_iter()
        .filter_map(|(held, p)| held.then_some(p))
        .collect()
    }

    pub fn names(&self) -> Vec<&'static str> {
        [
            (self.select, "select"),
            (self.insert, "insert"),
            (self.update, "update"),
            (self.delete, "delete"),
        ]
        .into_iter()
        .filter_map(|(held, name)| held.then_some(name))
        .collect()
    }
}

/// What one role can reach, by oid. Tables are listed only in schemas it has `USAGE` on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Visibility {
    pub schemas: HashSet<u32>,
    pub tables: HashMap<u32, Privileges>,
    pub functions: HashSet<u32>,
}

impl Visibility {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        #[derive(Deserialize)]
        struct RawVisibility {
            schemas: Vec<u32>,
            tables: Vec<(u32, bool, bool, bool, bool)>,
            functions: Vec<u32>,
        }
        let raw: RawVisibility = serde_json::from_str(json)?;
        Ok(Visibility {
            schemas: raw.schemas.into_iter().collect(),
            tables: raw
                .tables
                .into_iter()
                .map(|(oid, select, insert, update, delete)| {
                    (
                        oid,
                        Privileges {
                            select,
                            insert,
                            update,
                            delete,
                        },
                    )
                })
                .collect(),
            functions: raw.functions.into_iter().collect(),
        })
    }

    pub fn table(&self, oid: u32) -> Option<Privileges> {
        self.tables.get(&oid).copied().filter(Privileges::any)
    }
}

/// A database at one schema version.
pub struct Schema {
    pub version: String,
    namespaces: Vec<(u32, String)>,
    pub tables: Vec<Table>,
    pub functions: Vec<Function>,
    /// `(schema, table)` of the branch's app users table, when it has one.
    pub app_users: Option<(String, String)>,
    /// Enum type oid → (schema, name, labels in order).
    enums: HashMap<u32, (String, String, Vec<String>)>,
    views: moka::sync::Cache<String, Arc<RoleView>>,
}

impl std::fmt::Debug for Schema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schema")
            .field("version", &self.version)
            .field("tables", &self.tables.len())
            .field("functions", &self.functions.len())
            .finish_non_exhaustive()
    }
}

struct Types(HashMap<u32, RawType>);

impl Types {
    fn get(&self, oid: u32) -> Option<&RawType> {
        self.0.get(&oid)
    }

    /// A true array type (`typelem` set and variable length), as Postgres defines it.
    fn is_array(t: &RawType) -> bool {
        t.4 != 0 && t.7 == -1
    }

    /// Follows domains to their base type.
    fn base(&self, oid: u32) -> Option<&RawType> {
        let mut t = self.get(oid)?;
        for _ in 0..32 {
            if t.3 != "d" {
                return Some(t);
            }
            t = self.get(t.5)?;
        }
        None
    }

    fn desc(&self, oid: u32) -> Option<TypeDesc> {
        let mut t = self.base(oid)?;
        let array = Self::is_array(t);
        if array {
            t = self.base(t.4)?;
        }
        Some(TypeDesc {
            name: t.2.clone(),
            array,
            enum_ref: (t.3 == "e").then(|| format!("{}.{}", t.1, t.2)),
        })
    }

    /// The type as SQL: `format_type`'s spelling for Postgres's own types (`formatted`
    /// carries a column's typmod), and a quoted, qualified name for everything else, so a
    /// cast never depends on the search path.
    fn sql(&self, oid: u32, formatted: Option<&str>) -> Option<String> {
        let t = self.get(oid)?;
        if t.1 == "pg_catalog" {
            return Some(formatted.unwrap_or(&t.8).to_owned());
        }
        if Self::is_array(t) {
            let elem = self.get(t.4)?;
            if elem.1 != "pg_catalog" {
                return Some(format!(
                    "{}.{}[]",
                    quote_ident(&elem.1),
                    quote_ident(&elem.2)
                ));
            }
            return Some(formatted.unwrap_or(&t.8).to_owned());
        }
        Some(format!("{}.{}", quote_ident(&t.1), quote_ident(&t.2)))
    }
}

impl Schema {
    pub fn build(version: String, raw: Raw) -> Schema {
        let types = Types(raw.types.into_iter().map(|t| (t.0, t)).collect());
        let mut enums: HashMap<u32, (String, String, Vec<String>)> = HashMap::new();
        for (oid, label) in raw.enums {
            if let Some(t) = types.get(oid) {
                enums
                    .entry(oid)
                    .or_insert_with(|| (t.1.clone(), t.2.clone(), Vec::new()))
                    .2
                    .push(label);
            }
        }
        let tables = raw
            .relations
            .into_iter()
            .filter(|r| !is_reserved(&r.1))
            .map(
                |(oid, schema, name, relkind, rls, security_invoker, columns, primary_key)| Table {
                    oid,
                    schema,
                    name,
                    kind: match relkind.as_str() {
                        "v" => TableKind::View,
                        "m" => TableKind::MaterializedView,
                        "f" => TableKind::ForeignTable,
                        _ => TableKind::Table,
                    },
                    rls,
                    security_invoker,
                    columns: columns
                        .into_iter()
                        .map(
                            |(name, typid, formatted, nullable, has_default, generated)| {
                                let ty = types.desc(typid).unwrap_or(TypeDesc {
                                    name: formatted.clone(),
                                    array: false,
                                    enum_ref: None,
                                });
                                let text_like =
                                    !ty.array && types.base(typid).is_some_and(|b| b.6 == "S");
                                Column {
                                    info: ColumnInfo {
                                        type_sql: types
                                            .sql(typid, Some(&formatted))
                                            .unwrap_or(formatted),
                                        name,
                                        array: ty.array,
                                        text_like,
                                        generated,
                                    },
                                    ty,
                                    nullable,
                                    has_default,
                                }
                            },
                        )
                        .collect(),
                    primary_key,
                },
            )
            .collect();
        let functions = raw
            .functions
            .into_iter()
            .filter(|f| !is_reserved(&f.1))
            .filter_map(|f| callable(&types, f))
            .collect();
        Schema {
            version,
            namespaces: raw
                .namespaces
                .into_iter()
                .filter(|(_, n)| !is_reserved(n))
                .collect(),
            tables,
            functions,
            app_users: raw.app_users,
            enums,
            views: moka::sync::Cache::new(ROLE_VIEWS),
        }
    }

    /// The cached view for `role`, if it was computed at this version.
    pub fn cached_view(&self, role: &str) -> Option<Arc<RoleView>> {
        self.views.get(role)
    }

    /// Builds and caches the view for `role` from its [`Visibility`].
    pub fn insert_view(&self, role: &str, visibility: Visibility) -> Arc<RoleView> {
        let view = Arc::new(self.view(visibility));
        self.views.insert(role.to_owned(), view.clone());
        view
    }

    pub fn view(&self, visibility: Visibility) -> RoleView {
        let mut tables = HashMap::new();
        for t in &self.tables {
            if visibility.table(t.oid).is_some() {
                tables.insert(
                    (t.schema.clone(), t.name.clone()),
                    TableInfo {
                        schema: t.schema.clone(),
                        name: t.name.clone(),
                        columns: t.columns.iter().map(|c| c.info.clone()).collect(),
                    },
                );
            }
        }
        let mut functions: HashMap<(String, String), Overloads> = HashMap::new();
        for f in &self.functions {
            if visibility.functions.contains(&f.oid) {
                functions
                    .entry((f.info.schema.clone(), f.info.name.clone()))
                    .or_default()
                    .push((f.info.clone(), f.required().map(str::to_owned).collect()));
            }
        }
        RoleView {
            tables,
            functions,
            visibility,
        }
    }

    /// The `DataApiSchema` JSON: everything any of `roles` can reach. With `privileges`,
    /// each table lists what each role holds; without, `privileges` is null.
    pub fn to_json(
        &self,
        default_schema: &str,
        roles: &[(&str, &Visibility)],
        privileges: bool,
    ) -> Value {
        let mut schemas: BTreeMap<String, SchemaJson> = BTreeMap::new();
        for (oid, name) in &self.namespaces {
            if roles.iter().any(|(_, v)| v.schemas.contains(oid)) {
                schemas.entry(name.clone()).or_default();
            }
        }
        let mut referenced: HashSet<String> = HashSet::new();
        let mut note = |ty: &TypeDesc| {
            if let Some(e) = &ty.enum_ref {
                referenced.insert(e.clone());
            }
        };
        for t in &self.tables {
            let held: Vec<(&str, Privileges)> = roles
                .iter()
                .map(|(role, v)| (*role, v.table(t.oid).unwrap_or_default()))
                .collect();
            if !held.iter().any(|(_, p)| p.any()) {
                continue;
            }
            let mut columns = BTreeMap::new();
            for c in &t.columns {
                note(&c.ty);
                columns.insert(
                    c.info.name.clone(),
                    ColumnDesc {
                        ty: c.ty.name.clone(),
                        array: c.ty.array,
                        nullable: c.nullable,
                        has_default: c.has_default,
                        generated: c.info.generated,
                        r#enum: c.ty.enum_ref.clone(),
                    },
                );
            }
            let privileges = privileges.then(|| {
                held.iter()
                    .map(|(role, p)| ((*role).to_owned(), p.held()))
                    .collect()
            });
            schemas.entry(t.schema.clone()).or_default().0.insert(
                t.name.clone(),
                TableDesc {
                    kind: t.kind,
                    rls: t.rls,
                    security_invoker: t.security_invoker,
                    privileges,
                    columns,
                    primary_key: t.primary_key.clone(),
                },
            );
        }
        for f in &self.functions {
            if !roles.iter().any(|(_, v)| v.functions.contains(&f.oid)) {
                continue;
            }
            note(&f.returns);
            let args: Vec<ArgDesc> = f
                .args
                .iter()
                .map(|a| {
                    note(&a.ty);
                    ArgDesc {
                        name: a.name.clone(),
                        ty: a.ty.name.clone(),
                        array: a.ty.array,
                        has_default: a.has_default,
                    }
                })
                .collect();
            schemas
                .entry(f.info.schema.clone())
                .or_default()
                .1
                .entry(f.info.name.clone())
                .or_default()
                .push(FunctionDesc {
                    args,
                    returns: ReturnDesc {
                        ty: f.returns.name.clone(),
                        array: f.returns.array,
                        set: f.info.returns_set,
                    },
                });
        }
        let mut enums: BTreeMap<String, BTreeMap<String, Vec<String>>> = BTreeMap::new();
        for (schema, name, labels) in self.enums.values() {
            let qualified = format!("{schema}.{name}");
            if (schemas.contains_key(schema) || referenced.contains(&qualified))
                && !is_reserved(schema)
            {
                enums
                    .entry(schema.clone())
                    .or_default()
                    .insert(name.clone(), labels.clone());
            }
        }
        for schema in enums.keys() {
            schemas.entry(schema.clone()).or_default();
        }
        let out: BTreeMap<String, SchemaEntry> = schemas
            .into_iter()
            .map(|(name, (tables, functions))| {
                let enums = enums.remove(&name).unwrap_or_default();
                (
                    name,
                    SchemaEntry {
                        tables,
                        functions,
                        enums,
                    },
                )
            })
            .collect();
        serde_json::to_value(DataApiSchema {
            version: self.version.clone(),
            default_schema: default_schema.to_owned(),
            schemas: out,
            app_users: self
                .app_users
                .as_ref()
                .map(|(schema, table)| AppUsersTable {
                    schema: schema.clone(),
                    table: table.clone(),
                }),
        })
        .expect("the schema document serialises")
    }
}

/// The function as the API sees it, or `None` when it can't be called with named
/// arguments (see the module docs).
fn callable(types: &Types, f: RawFunction) -> Option<Function> {
    let (
        oid,
        schema,
        name,
        prokind,
        retset,
        rettype,
        argtypes,
        allargtypes,
        modes,
        names,
        nargdefaults,
        variadic,
        extension,
    ) = f;
    if prokind != "f" || variadic || extension {
        return None;
    }
    let all = allargtypes.unwrap_or(argtypes);
    let mut inputs = Vec::new();
    for (i, typ) in all.iter().enumerate() {
        let mode = modes
            .as_ref()
            .and_then(|m| m.get(i))
            .map_or("i", String::as_str);
        if !matches!(mode, "i" | "b" | "v") {
            continue;
        }
        let arg_name = names
            .as_ref()
            .and_then(|n| n.get(i))
            .filter(|n| !n.is_empty())?;
        let t = types.get(*typ)?;
        if t.3 == "p" {
            return None;
        }
        inputs.push((arg_name.clone(), *typ));
    }
    let ret = types.get(rettype)?;
    if ret.3 == "p" && !matches!(ret.2.as_str(), "void" | "record") {
        return None;
    }
    let defaults = usize::try_from(nargdefaults).unwrap_or(0);
    let first_default = inputs.len().saturating_sub(defaults);
    let mut args = Vec::new();
    let mut info_args = Vec::new();
    for (i, (arg_name, typ)) in inputs.into_iter().enumerate() {
        info_args.push((arg_name.clone(), types.sql(typ, None)?));
        args.push(Arg {
            name: arg_name,
            ty: types.desc(typ)?,
            has_default: i >= first_default,
        });
    }
    Some(Function {
        oid,
        info: FunctionInfo {
            schema,
            name,
            args: info_args,
            returns_set: retset,
            returns_void: ret.1 == "pg_catalog" && ret.2 == "void",
        },
        args,
        returns: types.desc(rettype)?,
    })
}

/// What one role can reach at one schema version: the planner's catalog.
#[derive(Debug)]
pub struct RoleView {
    tables: HashMap<(String, String), TableInfo>,
    /// Each overload with the names of its arguments that have no default.
    functions: HashMap<(String, String), Overloads>,
    pub visibility: Visibility,
}

impl Catalog for RoleView {
    fn table(&self, name: &Name) -> Option<&TableInfo> {
        let schema = name.schema.as_ref()?;
        self.tables.get(&(schema.clone(), name.name.clone()))
    }

    fn function(&self, name: &Name, arg_names: &[&str]) -> Result<&FunctionInfo, FnLookupError> {
        let schema = name.schema.as_ref().ok_or(FnLookupError::NotFound)?;
        let overloads = self
            .functions
            .get(&(schema.clone(), name.name.clone()))
            .ok_or(FnLookupError::NotFound)?;
        let mut found = overloads.iter().filter(|(f, required)| {
            arg_names.iter().all(|a| f.args.iter().any(|(n, _)| n == a))
                && required.iter().all(|r| arg_names.contains(&r.as_str()))
        });
        match (found.next(), found.next()) {
            (None, _) => Err(FnLookupError::NotFound),
            (Some((f, _)), None) => Ok(f),
            (Some(_), Some(_)) => Err(FnLookupError::Ambiguous),
        }
    }
}

/// Reads the catalog at `version` (inside the request's transaction).
pub async fn introspect(client: &Client, version: String) -> Result<Schema, IntrospectError> {
    let json: String = client
        .query_typed_one(INTROSPECT_SQL, &[(&RESERVED_SCHEMAS, Type::TEXT_ARRAY)])
        .await?
        .try_get(0)?;
    let mut raw: Raw = serde_json::from_str(&json)?;
    raw.app_users = match client.query_typed_opt(APP_USERS_TABLE_SQL, &[]).await? {
        Some(row) => Some((row.try_get(0)?, row.try_get(1)?)),
        None => None,
    };
    Ok(Schema::build(version, raw))
}

/// The app users table, from the catalogs alone: the table carrying the guard trigger that
/// calls `chelabase.users_guard()`. Introspection runs as the request's role, which may have no
/// access to schema `chelabase` (a custom role), and the schema it builds is shared by every
/// role, so the marker can't depend on what the role may read.
pub const APP_USERS_TABLE_SQL: &str = "SELECT n.nspname::text, c.relname::text \
     FROM pg_catalog.pg_trigger t \
     JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.tgrelid \
     JOIN pg_catalog.pg_namespace n ON n.oid OPERATOR(pg_catalog.=) c.relnamespace \
     JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) t.tgfoid \
     JOIN pg_catalog.pg_namespace pn ON pn.oid OPERATOR(pg_catalog.=) p.pronamespace \
     WHERE t.tgname OPERATOR(pg_catalog.=) 'chelabase_users_guard' \
       AND p.proname OPERATOR(pg_catalog.=) 'users_guard' \
       AND pn.nspname OPERATOR(pg_catalog.=) 'chelabase' \
     ORDER BY c.oid LIMIT 1";

/// What `role` can reach (inside the request's transaction).
pub async fn visibility(client: &Client, role: &str) -> Result<Visibility, IntrospectError> {
    let json: String = client
        .query_typed_one(
            VISIBILITY_SQL,
            &[(&role, Type::TEXT), (&RESERVED_SCHEMAS, Type::TEXT_ARRAY)],
        )
        .await?
        .try_get(0)?;
    Ok(Visibility::from_json(&json)?)
}

/// The view for `role` at `schema`'s version, computed once and cached with the schema.
pub async fn role_view(
    client: &Client,
    schema: &Schema,
    role: &str,
) -> Result<Arc<RoleView>, IntrospectError> {
    if let Some(view) = schema.cached_view(role) {
        return Ok(view);
    }
    let visibility = visibility(client, role).await?;
    Ok(schema.insert_view(role, visibility))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn td(name: &str, array: bool, enum_ref: Option<&str>) -> TypeDesc {
        TypeDesc {
            name: name.into(),
            array,
            enum_ref: enum_ref.map(Into::into),
        }
    }

    fn column(name: &str, ty: TypeDesc, nullable: bool, has_default: bool, generated: bool) -> Column {
        Column {
            info: ColumnInfo {
                name: name.into(),
                type_sql: ty.name.clone(),
                array: ty.array,
                text_like: false,
                generated,
            },
            ty,
            nullable,
            has_default,
        }
    }

    fn table(oid: u32, schema: &str, name: &str, kind: TableKind, invoker: bool, columns: Vec<Column>) -> Table {
        Table {
            oid,
            schema: schema.into(),
            name: name.into(),
            kind,
            rls: !invoker,
            security_invoker: invoker,
            columns,
            primary_key: vec![],
        }
    }

    fn fixture() -> Schema {
        let search = Function {
            oid: 20,
            info: FunctionInfo {
                schema: "public".into(),
                name: "search".into(),
                args: vec![("q".into(), "text".into()), ("lim".into(), "integer".into())],
                returns_set: true,
                returns_void: false,
            },
            args: vec![
                Arg { name: "q".into(), ty: td("text", false, None), has_default: false },
                Arg { name: "lim".into(), ty: td("int4", false, None), has_default: true },
            ],
            returns: td("text", true, None),
        };
        let mut enums = HashMap::new();
        enums.insert(100, ("billing".to_owned(), "mood".to_owned(), vec!["sad".to_owned(), "happy".to_owned()]));
        enums.insert(101, ("extra".to_owned(), "level".to_owned(), vec!["low".to_owned(), "high".to_owned()]));
        enums.insert(102, ("other".to_owned(), "unused".to_owned(), vec!["x".to_owned()]));
        Schema {
            version: "17.2".into(),
            app_users: None,
            namespaces: vec![(1, "public".into()), (2, "billing".into())],
            tables: vec![
                table(
                    10,
                    "public",
                    "items",
                    TableKind::Table,
                    false,
                    vec![
                        column("id", td("int8", false, None), false, true, true),
                        column("tags", td("text", true, None), true, false, false),
                        column("mood", td("mood", false, Some("billing.mood")), false, false, false),
                        column("level", td("level", false, Some("extra.level")), true, false, false),
                    ],
                ),
                table(
                    11,
                    "billing",
                    "v",
                    TableKind::View,
                    true,
                    vec![column("amount", td("numeric", false, None), true, false, false)],
                ),
            ],
            functions: vec![search],
            enums,
            views: moka::sync::Cache::new(ROLE_VIEWS),
        }
    }

    fn visibility(tables: &[(u32, Privileges)]) -> Visibility {
        Visibility {
            schemas: HashSet::from([1, 2]),
            tables: tables.iter().copied().collect(),
            functions: HashSet::from([20]),
        }
    }

    fn expected(anon: Value, v_privs: Value) -> Value {
        json!({
            "version": "17.2",
            "default_schema": "public",
            "schemas": {
                "public": {
                    "tables": {"items": {
                        "kind": "table", "rls": true, "security_invoker": false, "privileges": anon,
                        "columns": {
                            "id": {"type": "int8", "array": false, "nullable": false, "has_default": true, "generated": true},
                            "tags": {"type": "text", "array": true, "nullable": true, "has_default": false, "generated": false},
                            "mood": {"type": "mood", "array": false, "nullable": false, "has_default": false, "generated": false, "enum": "billing.mood"},
                            "level": {"type": "level", "array": false, "nullable": true, "has_default": false, "generated": false, "enum": "extra.level"}
                        }
                    }},
                    "functions": {"search": [{
                        "args": [
                            {"name": "q", "type": "text", "array": false, "has_default": false},
                            {"name": "lim", "type": "int4", "array": false, "has_default": true}
                        ],
                        "returns": {"type": "text", "array": true, "set": true}
                    }]},
                    "enums": {}
                },
                "billing": {
                    "tables": {"v": {
                        "kind": "view", "rls": false, "security_invoker": true, "privileges": v_privs,
                        "columns": {"amount": {"type": "numeric", "array": false, "nullable": true, "has_default": false, "generated": false}}
                    }},
                    "functions": {},
                    "enums": {"mood": ["sad", "happy"]}
                },
                "extra": {"tables": {}, "functions": {}, "enums": {"level": ["low", "high"]}}
            }
        })
    }

    #[test]
    fn to_json_matches_the_documented_document() {
        let s = fixture();
        let select = Privileges { select: true, ..Default::default() };
        let all = Privileges { select: true, insert: true, update: true, delete: true };
        let anon = visibility(&[(10, select)]);
        let auth = visibility(&[(10, all), (11, select)]);
        let roles = [("anon", &anon), ("authenticated", &auth)];

        let without = s.to_json("public", &roles, false);
        assert_eq!(without, expected(Value::Null, Value::Null), "{without}");

        let with = s.to_json("public", &roles, true);
        let items = json!({"anon": ["select"], "authenticated": ["select", "insert", "update", "delete"]});
        let v = json!({"anon": [], "authenticated": ["select"]});
        assert_eq!(with, expected(items, v), "{with}");
    }
}
