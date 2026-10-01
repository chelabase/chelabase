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

use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::query::Name;
use crate::sql::{Catalog, ColumnInfo, FnLookupError, FunctionInfo, TableInfo, quote_ident};

/// Schemas never exposed, whatever the grants: the platform's machinery, Postgres's and
/// Neon's own. `pg_temp_*` and `pg_toast_temp_*` are hidden too ([`is_reserved`]).
pub const RESERVED_SCHEMAS: &[&str] = &[
    "cheladb",
    "pg_catalog",
    "information_schema",
    "pg_toast",
    "neon",
    "neon_migration",
];

/// Each overload of one function, with the names of its arguments that have no default.
type Overloads = Vec<(FunctionInfo, Vec<String>)>;
/// A schema's tables and its functions' overloads, for the JSON.
type SchemaJson = (Map<String, Value>, BTreeMap<String, Vec<Value>>);

/// Roles cached per schema version; past this many, the oldest are dropped.
const ROLE_VIEWS: u64 = 64;

pub fn is_reserved(schema: &str) -> bool {
    RESERVED_SCHEMAS.contains(&schema)
        || schema.starts_with("pg_temp_")
        || schema.starts_with("pg_toast_temp_")
}

/// `$1` = [`RESERVED_SCHEMAS`]. One row, one `text` column: the JSON [`Raw`] reads (oids
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
        FROM pg_catalog.pg_attribute a WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped))), '[]')
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

/// `$1` = the role, `$2` = [`RESERVED_SCHEMAS`]. One row, one `text` column: the JSON
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
    Db(#[from] sqlx::Error),
    #[error("reading the catalog's JSON: {0}")]
    Decode(#[from] serde_json::Error),
}

/// `(oid, schema, name, typtype, typelem, typbasetype, typcategory, typlen, format_type)`.
type RawType = (u32, String, String, String, u32, u32, String, i16, String);
/// `(name, type oid, format_type with typmod, nullable, has_default, generated)`.
type RawColumn = (String, u32, String, bool, bool, bool);
/// `(oid, schema, name, relkind, rls, security_invoker, columns)`. `security_invoker` is
/// true only for a view whose options set it (any spelling Postgres reads as true: the
/// value is cast to `boolean`).
type RawRelation = (u32, String, String, String, bool, bool, Vec<RawColumn>);
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
    pub kind: &'static str,
    pub rls: bool,
    /// A view that runs with the caller's privileges (and so under its RLS).
    pub security_invoker: bool,
    pub columns: Vec<Column>,
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
                |(oid, schema, name, relkind, rls, security_invoker, columns)| Table {
                    oid,
                    schema,
                    name,
                    kind: match relkind.as_str() {
                        "v" => "view",
                        "m" => "materialized_view",
                        "f" => "foreign_table",
                        _ => "table",
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
            let mut columns = Map::new();
            for c in &t.columns {
                note(&c.ty);
                let mut column = json!({
                    "type": c.ty.name,
                    "array": c.ty.array,
                    "nullable": c.nullable,
                    "has_default": c.has_default,
                    "generated": c.info.generated,
                });
                if let Some(e) = &c.ty.enum_ref {
                    column["enum"] = json!(e);
                }
                columns.insert(c.info.name.clone(), column);
            }
            let privileges = if privileges {
                Value::Object(
                    held.iter()
                        .map(|(role, p)| ((*role).to_owned(), json!(p.names())))
                        .collect(),
                )
            } else {
                Value::Null
            };
            schemas.entry(t.schema.clone()).or_default().0.insert(
                t.name.clone(),
                json!({
                    "kind": t.kind,
                    "rls": t.rls,
                    "security_invoker": t.security_invoker,
                    "privileges": privileges,
                    "columns": columns,
                }),
            );
        }
        for f in &self.functions {
            if !roles.iter().any(|(_, v)| v.functions.contains(&f.oid)) {
                continue;
            }
            note(&f.returns);
            let args: Vec<Value> = f
                .args
                .iter()
                .map(|a| {
                    note(&a.ty);
                    json!({ "name": a.name, "type": a.ty.name, "array": a.ty.array, "has_default": a.has_default })
                })
                .collect();
            schemas
                .entry(f.info.schema.clone())
                .or_default()
                .1
                .entry(f.info.name.clone())
                .or_default()
                .push(json!({
                    "args": args,
                    "returns": { "type": f.returns.name, "array": f.returns.array, "set": f.info.returns_set },
                }));
        }
        let mut enums: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        for (schema, name, labels) in self.enums.values() {
            let qualified = format!("{schema}.{name}");
            if (schemas.contains_key(schema) || referenced.contains(&qualified))
                && !is_reserved(schema)
            {
                enums
                    .entry(schema.clone())
                    .or_default()
                    .insert(name.clone(), json!(labels));
            }
        }
        for schema in enums.keys() {
            schemas.entry(schema.clone()).or_default();
        }
        let out: Map<String, Value> = schemas
            .into_iter()
            .map(|(name, (tables, functions))| {
                let e = enums.remove(&name).unwrap_or_default();
                (
                    name,
                    json!({ "tables": tables, "functions": functions, "enums": e }),
                )
            })
            .collect();
        json!({ "version": self.version, "default_schema": default_schema, "schemas": out })
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
pub async fn introspect(
    conn: &mut PgConnection,
    version: String,
) -> Result<Schema, IntrospectError> {
    let json: String = sqlx::query_scalar(INTROSPECT_SQL)
        .bind(RESERVED_SCHEMAS)
        .fetch_one(&mut *conn)
        .await?;
    Ok(Schema::build(version, serde_json::from_str(&json)?))
}

/// What `role` can reach (inside the request's transaction).
pub async fn visibility(
    conn: &mut PgConnection,
    role: &str,
) -> Result<Visibility, IntrospectError> {
    let json: String = sqlx::query_scalar(VISIBILITY_SQL)
        .bind(role)
        .bind(RESERVED_SCHEMAS)
        .fetch_one(&mut *conn)
        .await?;
    Ok(Visibility::from_json(&json)?)
}

/// The view for `role` at `schema`'s version, computed once and cached with the schema.
pub async fn role_view(
    conn: &mut PgConnection,
    schema: &Schema,
    role: &str,
) -> Result<Arc<RoleView>, IntrospectError> {
    if let Some(view) = schema.cached_view(role) {
        return Ok(view);
    }
    let visibility = visibility(conn, role).await?;
    Ok(schema.insert_view(role, visibility))
}
