//! The data API's schema JSON (`GET /data/v1/schema`) to `@chelabase/client`'s types: the
//! TypeScript module `chelabase gen types` writes, or the runtime `types` map alone as JSON.
//!
//! The module is `Database` (the shape `@chelabase/client`'s `DatabaseShape` wants) plus
//! `types`, the runtime map of which columns and returns are `int8` or `numeric`. Keys are
//! sorted, so the same schema always gives the same text. Nothing here does I/O: callers
//! fetch the schemas ([`collect`] runs `gen types`' fetch plan through a caller's fetch),
//! then [`render`] them.
//!
//! ```no_run
//! # async fn fetch(database: Option<String>) -> Result<chelabase_typegen::Schema, String> { todo!() }
//! # async fn demo() -> Result<(), String> {
//! use chelabase_typegen::{Format, Which, collect, render};
//! let which = Which::from_options(None, false).map_err(|e| e.to_string())?;
//! let schemas = collect(&which, fetch).await?;
//! let db_ts = render(&schemas, Format::Ts).map_err(|e| e.to_string())?;
//! # let _ = db_ts; Ok(()) }
//! ```

use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Why a schema can't be rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A schema whose name has a dot: the data API's names (`"schema.table"`) can't address it.
    DottedSchema { schema: String },
    /// One of several databases failed (`source` says why).
    Database {
        database: String,
        source: Box<Error>,
    },
    /// [`render`] was given no schema at all.
    NoSchemas,
    /// Two of several databases share a key: with every database, main is keyed `main`, so
    /// another database named `main` clashes with it (#29).
    DuplicateDatabase { database: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::DottedSchema { schema } => write!(
                f,
                "schema {} has a dot in its name, which the data API's names (\"schema.table\") can't address; rename it, or revoke the key's role's USAGE on it",
                quote(schema)
            ),
            Error::Database { database, .. } => write!(f, "database {}", quote(database)),
            Error::NoSchemas => f.write_str("no schema to generate from"),
            Error::DuplicateDatabase { database } if database == "main" => f.write_str(
                "another database is named \"main\", the key main's types always take with every database; generate it alone (`database=main`, or `--database main`) instead of all databases",
            ),
            Error::DuplicateDatabase { database } => write!(
                f,
                "the database {} is listed twice; generate it alone (`database=`) instead of all databases",
                quote(database)
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Database { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// This crate's results.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// `GET /data/v1/schema`.
#[derive(Debug, Deserialize)]
pub struct Schema {
    version: String,
    default_schema: String,
    schemas: BTreeMap<String, SchemaPart>,
    /// The databases the key may reach (main's response only; older servers omit it).
    #[serde(default)]
    databases: Vec<String>,
    /// The table app users are mirrored into, when the branch has one.
    #[serde(default)]
    app_users: Option<AppUsersRef>,
}

#[derive(Debug, Deserialize)]
struct AppUsersRef {
    schema: String,
    table: String,
}

#[derive(Debug, Default, Deserialize)]
struct SchemaPart {
    #[serde(default)]
    tables: BTreeMap<String, Table>,
    #[serde(default)]
    functions: BTreeMap<String, Vec<Function>>,
    #[serde(default)]
    enums: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Table {
    columns: BTreeMap<String, Column>,
}

#[derive(Debug, Deserialize)]
struct Column {
    #[serde(rename = "type")]
    ty: String,
    array: bool,
    nullable: bool,
    has_default: bool,
    generated: bool,
    #[serde(rename = "enum")]
    enum_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Function {
    args: Vec<Arg>,
    returns: Returns,
}

#[derive(Debug, Deserialize)]
struct Arg {
    name: String,
    #[serde(rename = "type")]
    ty: String,
    array: bool,
    has_default: bool,
}

#[derive(Debug, Deserialize)]
struct Returns {
    #[serde(rename = "type")]
    ty: String,
    array: bool,
    set: bool,
}

impl Schema {
    /// The databases the key may reach, main first (main's response only; empty when the
    /// server didn't say).
    pub fn databases(&self) -> &[String] {
        &self.databases
    }
}

/// The schema's name for a `storage.object_ref` column (the storage schema's composite of
/// `bucket_id` and `name`), typed `StorageRef`.
const STORAGE_REF_TYPE: &str = "object_ref";

/// `int8` or `numeric`: what the runtime map tracks.
fn exact(ty: &str) -> Option<&'static str> {
    match ty {
        "int8" => Some("int8"),
        "numeric" => Some("numeric"),
        _ => None,
    }
}

/// A TS property name: bare when it's an identifier, else a quoted string.
fn key(name: &str) -> String {
    let mut chars = name.chars();
    let ident = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$');
    if ident { name.to_owned() } else { quote(name) }
}

/// `s` as a TS (and JSON) string literal. A JSON string is a TS string literal once U+2028
/// and U+2029 are escaped (JSON allows them raw; in JS they end a line, and so a comment).
pub fn quote(s: &str) -> String {
    serde_json::to_string(s)
        .expect("a string serialises")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// Text for a `//` comment: escaped like a string literal's contents, so no line break
/// (`\n`, `\r`, U+2028, U+2029) ends the comment early.
fn comment(s: &str) -> String {
    let q = quote(s);
    q[1..q.len() - 1].to_owned()
}

/// A property name in the runtime map (an object literal): `__proto__` there would set
/// the prototype, so it becomes a computed key.
fn rt_key(name: &str) -> String {
    if name == "__proto__" {
        format!("[{}]", quote(name))
    } else {
        key(name)
    }
}

struct Ctx<'a> {
    schema: &'a Schema,
    uses_int8: bool,
    uses_json: bool,
    uses_storage_ref: bool,
}

impl Ctx<'_> {
    /// The TS type of a base type (no array, no null).
    fn scalar(&mut self, ty: &str, enum_labels: Option<&[String]>) -> String {
        if let Some(labels) = enum_labels {
            return match labels {
                [] => "never".into(),
                _ => labels
                    .iter()
                    .map(|l| quote(l))
                    .collect::<Vec<_>>()
                    .join(" | "),
            };
        }
        match ty {
            "int8" => {
                self.uses_int8 = true;
                "Int8".into()
            }
            "int2" | "int4" | "float4" | "float8" => "number".into(),
            "bool" => "boolean".into(),
            "json" | "jsonb" => {
                self.uses_json = true;
                "Json".into()
            }
            // `storage.object_ref` (object-storage m3): the data API answers it as an object.
            STORAGE_REF_TYPE => {
                self.uses_storage_ref = true;
                "StorageRef".into()
            }
            // numeric, text-like, uuid, dates and times, and anything else
            _ => "string".into(),
        }
    }

    fn typed(&mut self, ty: &str, array: bool, enum_labels: Option<&[String]>) -> String {
        let base = self.scalar(ty, enum_labels);
        match (array, base.contains(" | ")) {
            (false, _) => base,
            (true, false) => format!("{base}[]"),
            (true, true) => format!("({base})[]"),
        }
    }

    /// The labels of the enum a column names (`"schema.name"`).
    fn column_enum(&self, c: &Column) -> Option<&[String]> {
        let (schema, name) = c.enum_ref.as_deref()?.split_once('.')?;
        self.schema
            .schemas
            .get(schema)?
            .enums
            .get(name)
            .map(Vec::as_slice)
    }

    /// An argument's or return's enum: the type name has no schema, so look in the
    /// function's own schema first, then in the one schema that has such an enum.
    fn named_enum(&self, own: &str, ty: &str) -> Option<&[String]> {
        if let Some(l) = self.schema.schemas.get(own).and_then(|s| s.enums.get(ty)) {
            return Some(l);
        }
        let mut found = self.schema.schemas.values().filter_map(|s| s.enums.get(ty));
        match (found.next(), found.next()) {
            (Some(l), None) => Some(l),
            _ => None,
        }
    }

    fn column(&mut self, c: &Column) -> String {
        let labels = self.column_enum(c).map(<[String]>::to_vec);
        let t = self.typed(&c.ty, c.array, labels.as_deref());
        if c.nullable { format!("{t} | null") } else { t }
    }

    /// `{ a: T; b?: U }` for a table's columns, per the Row/Insert/Update rules.
    fn fields(&mut self, table: &Table, mode: Mode) -> Vec<String> {
        let mut out = Vec::new();
        for (name, c) in &table.columns {
            if c.generated && mode != Mode::Row {
                continue;
            }
            let optional = match mode {
                Mode::Row => false,
                Mode::Insert => c.nullable || c.has_default,
                Mode::Update => true,
            };
            let t = self.column(c);
            out.push(format!(
                "{}{}: {t}",
                key(name),
                if optional { "?" } else { "" }
            ));
        }
        out
    }

    /// The TS `Args` type of one overload.
    fn args(&mut self, schema: &str, f: &Function) -> String {
        if f.args.is_empty() {
            return "Record<string, never>".into();
        }
        let mut args: Vec<&Arg> = f.args.iter().collect();
        args.sort_by(|a, b| a.name.cmp(&b.name));
        let fields: Vec<String> = args
            .iter()
            .map(|a| {
                let labels = self.named_enum(schema, &a.ty).map(<[String]>::to_vec);
                let t = self.typed(&a.ty, a.array, labels.as_deref());
                format!(
                    "{}{}: {t}",
                    key(&a.name),
                    if a.has_default { "?" } else { "" }
                )
            })
            .collect();
        format!("{{ {} }}", fields.join("; "))
    }

    /// The TS `Returns` type of one overload. A function returning a table or view's row
    /// type gets that row; `record` and other composites are `Json`.
    fn returns(&mut self, schema: &str, f: &Function) -> String {
        let r = &f.returns;
        let part = self.schema.schemas.get(schema);
        let t = if r.ty == "void" {
            return "null".into();
        } else if let Some(table) = part.and_then(|p| p.tables.get(&r.ty)) {
            let fields = self.fields(table, Mode::Row);
            if fields.is_empty() {
                "Record<string, never>".into()
            } else {
                format!("{{ {} }}", fields.join("; "))
            }
        } else if r.ty == "record" {
            self.uses_json = true;
            "Json".into()
        } else {
            let labels = self.named_enum(schema, &r.ty).map(<[String]>::to_vec);
            self.scalar(&r.ty, labels.as_deref())
        };
        let t = if t.contains(" | ") && !t.starts_with('{') {
            format!("({t})")
        } else {
            t
        };
        let t = if r.array { format!("{t}[]") } else { t };
        if r.set { format!("{t}[]") } else { t }
    }

    /// The runtime entry of a function's overloads (all typed alike): a scalar `returns`,
    /// or a table-returning function's int8/numeric `columns`. Overloads typed alike can
    /// still differ here (`text` and `numeric` are both strings), so every overload's entry
    /// is merged.
    fn function_runtime(&self, schema: &str, overloads: &[Function]) -> FnRt {
        let mut returns = None;
        let mut columns: BTreeMap<&str, &'static str> = BTreeMap::new();
        for f in overloads {
            if let Some(kind) = exact(&f.returns.ty) {
                returns.get_or_insert(kind);
            } else if let Some(t) = self
                .schema
                .schemas
                .get(schema)
                .and_then(|p| p.tables.get(&f.returns.ty))
            {
                for (name, c) in &t.columns {
                    if let Some(kind) = exact(&c.ty) {
                        columns.entry(name).or_insert(kind);
                    }
                }
            }
        }
        if let Some(kind) = returns {
            return FnRt::Returns(kind);
        }
        FnRt::Columns(
            columns
                .into_iter()
                .map(|(n, k)| (n.to_owned(), k))
                .collect(),
        )
    }
}

/// A function's runtime entry: one model, written as the module's literal and as JSON.
enum FnRt {
    /// `{ returns: "int8" }`
    Returns(&'static str),
    /// `{ columns: { … } }`, or `{}` when no column is int8 or numeric.
    Columns(BTreeMap<String, &'static str>),
}

impl FnRt {
    fn ts(&self) -> String {
        match self {
            FnRt::Returns(kind) => format!("{{ returns: {} }}", quote(kind)),
            FnRt::Columns(columns) if columns.is_empty() => "{}".into(),
            FnRt::Columns(columns) => format!("{{ columns: {} }}", columns_ts(columns)),
        }
    }

    fn json(&self) -> Value {
        let mut entry = Map::new();
        match self {
            FnRt::Returns(kind) => {
                entry.insert("returns".into(), Value::from(*kind));
            }
            FnRt::Columns(columns) if columns.is_empty() => {}
            FnRt::Columns(columns) => {
                entry.insert("columns".into(), columns_json(columns));
            }
        }
        Value::Object(entry)
    }
}

/// `{ id: "int8", total: "numeric" }` (non-empty).
fn columns_ts(columns: &BTreeMap<String, &'static str>) -> String {
    let cols: Vec<String> = columns
        .iter()
        .map(|(n, k)| format!("{}: {}", rt_key(n), quote(k)))
        .collect();
    format!("{{ {} }}", cols.join(", "))
}

fn columns_json(columns: &BTreeMap<String, &'static str>) -> Value {
    Value::Object(
        columns
            .iter()
            .map(|(n, k)| (n.clone(), Value::from(*k)))
            .collect(),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Row,
    Insert,
    Update,
}

/// A table's int8 and numeric columns.
fn exact_columns(t: &Table) -> BTreeMap<String, &'static str> {
    t.columns
        .iter()
        .filter_map(|(n, c)| exact(&c.ty).map(|k| (n.clone(), k)))
        .collect()
}

/// `{ id: "int8", total: "numeric" }`, or `{}`.
fn exact_columns_ts(columns: &BTreeMap<String, &'static str>) -> String {
    if columns.is_empty() {
        "{}".into()
    } else {
        columns_ts(columns)
    }
}

/// One database, rendered: the bodies of its `schemas` for the type and the runtime map.
struct Rendered {
    db: String,
    rt: String,
    /// `rt` as JSON: the whole `TypeMap` (`defaultSchema` and `schemas`).
    rt_json: Value,
    dflt: String,
    app_users: Option<String>,
    version: String,
    uses_int8: bool,
    uses_json: bool,
    uses_storage_ref: bool,
}

impl Rendered {
    /// `{ defaultSchema: …; schemas: {…}; }` (no trailing `;`).
    fn db_type(&self) -> String {
        let app_users = self
            .app_users
            .as_ref()
            .map(|a| format!("  appUsers: {a};\n"))
            .unwrap_or_default();
        format!(
            "{{\n  defaultSchema: {};\n  schemas: {{\n{}  }};\n{app_users}}}",
            self.dflt, self.db
        )
    }

    /// `{ defaultSchema: …, schemas: {…}, }` (a `TypeMap` literal).
    fn rt_object(&self) -> String {
        format!(
            "{{\n  defaultSchema: {},\n  schemas: {{\n{}  }},\n}}",
            self.dflt, self.rt
        )
    }
}

/// `import type { … } from "@chelabase/client";` for what the parts use.
fn header(parts: &[&Rendered], versions: &str) -> String {
    let mut imports = Vec::new();
    if parts.iter().any(|r| r.uses_int8) {
        imports.push("Int8");
    }
    if parts.iter().any(|r| r.uses_json) {
        imports.push("Json");
    }
    if parts.iter().any(|r| r.uses_storage_ref) {
        imports.push("StorageRef");
    }
    imports.push("TypeMap");
    format!(
        "// Generated by Chelabase from the data API schema. Don't edit; download it again when the schema changes.\n// Schema version: {versions}\nimport type {{ {} }} from \"@chelabase/client\";\n\n",
        imports.join(", ")
    )
}

/// Indents every line after the first by two spaces.
fn nest(text: &str) -> String {
    text.replace('\n', "\n  ")
}

/// The module text for one database: `Database` and `types`.
pub fn generate(schema: &Schema) -> Result<String> {
    let r = render_one(schema)?;
    let mut out = header(&[&r], &comment(&r.version));
    let _ = writeln!(out, "export type Database = {};\n", r.db_type());
    let _ = writeln!(
        out,
        "export const types = {} as const satisfies TypeMap;",
        r.rt_object()
    );
    Ok(out)
}

/// Several databases need a key each: none empty, none twice (#29).
fn distinct(all: &[(String, Schema)]) -> Result<()> {
    if all.is_empty() {
        return Err(Error::NoSchemas);
    }
    let mut seen = std::collections::BTreeSet::new();
    for (n, _) in all {
        if !seen.insert(n.as_str()) {
            return Err(Error::DuplicateDatabase {
                database: n.clone(),
            });
        }
    }
    Ok(())
}

/// The module text for several databases (main first, as given): `Databases` with a key per
/// database, `Database` as the first one, and `types` keyed alike.
pub fn generate_all(all: &[(String, Schema)]) -> Result<String> {
    distinct(all)?;
    let rendered = all
        .iter()
        .map(|(n, s)| {
            render_one(s).map_err(|e| Error::Database {
                database: n.clone(),
                source: Box::new(e),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let parts: Vec<&Rendered> = rendered.iter().collect();
    let versions = all
        .iter()
        .zip(&rendered)
        .map(|((n, _), r)| comment(&format!("{n} {}", r.version)))
        .collect::<Vec<_>>()
        .join(", ");
    let mut out = header(&parts, &versions);
    out.push_str("export type Databases = {\n");
    for ((n, _), r) in all.iter().zip(&rendered) {
        let _ = writeln!(out, "  {}: {};", key(n), nest(&r.db_type()));
    }
    out.push_str("};\n\n");
    let _ = writeln!(
        out,
        "export type Database = Databases[{}];\n",
        quote(&all[0].0)
    );
    out.push_str("export const types = {\n");
    for ((n, _), r) in all.iter().zip(&rendered) {
        let _ = writeln!(out, "  {}: {},", rt_key(n), nest(&r.rt_object()));
    }
    out.push_str("} as const satisfies Record<string, TypeMap>;\n");
    Ok(out)
}

fn render_one(schema: &Schema) -> Result<Rendered> {
    // A name is "table" or "schema.table", split at its one dot: a schema whose name has a
    // dot can't be addressed.
    if let Some(dotted) = schema.schemas.keys().find(|n| n.contains('.')) {
        return Err(Error::DottedSchema {
            schema: dotted.clone(),
        });
    }
    // The default schema must exist in the map, even with nothing in it.
    let mut empty = BTreeMap::new();
    let mut names: Vec<&String> = schema.schemas.keys().collect();
    if !schema.schemas.contains_key(&schema.default_schema) {
        empty.insert(schema.default_schema.clone(), SchemaPart::default());
        names.push(&schema.default_schema);
        names.sort();
    }
    let part = |n: &str| {
        schema
            .schemas
            .get(n)
            .or_else(|| empty.get(n))
            .expect("listed")
    };
    let mut cx = Ctx {
        schema,
        uses_int8: false,
        uses_json: false,
        uses_storage_ref: false,
    };

    let mut db = String::new();
    let mut rt = String::new();
    let mut rt_schemas = Map::new();
    for name in names {
        let p = part(name);
        let mut rt_tables = Map::new();
        let mut rt_functions = Map::new();
        let _ = writeln!(db, "    {}: {{", key(name));
        let _ = writeln!(rt, "    {}: {{", rt_key(name));
        // tables and views
        if p.tables.is_empty() {
            db.push_str("      tables: {};\n");
            rt.push_str("      tables: {},\n");
        } else {
            db.push_str("      tables: {\n");
            rt.push_str("      tables: {\n");
            for (tn, t) in &p.tables {
                let _ = writeln!(db, "        {}: {{", key(tn));
                for (label, mode) in [
                    ("Row", Mode::Row),
                    ("Insert", Mode::Insert),
                    ("Update", Mode::Update),
                ] {
                    let fields = cx.fields(t, mode);
                    if fields.is_empty() {
                        let _ = writeln!(db, "          {label}: Record<string, never>;");
                    } else {
                        let _ = writeln!(db, "          {label}: {{");
                        for f in fields {
                            let _ = writeln!(db, "            {f};");
                        }
                        db.push_str("          };\n");
                    }
                }
                db.push_str("        };\n");
                let columns = exact_columns(t);
                let _ = writeln!(
                    rt,
                    "        {}: {},",
                    rt_key(tn),
                    exact_columns_ts(&columns)
                );
                rt_tables.insert(tn.clone(), columns_json(&columns));
            }
            db.push_str("      };\n");
            rt.push_str("      },\n");
        }
        // functions: overloads with one return type become a union of their Args; overloads
        // that return different types can't be typed apart, so they're left out.
        let mut fns = String::new();
        let mut fns_rt = String::new();
        for (fname, overloads) in &p.functions {
            let returns: Vec<String> = overloads.iter().map(|f| cx.returns(name, f)).collect();
            if returns.is_empty() {
                continue;
            }
            if returns.iter().any(|r| *r != returns[0]) {
                let _ = writeln!(
                    fns,
                    "        // {} is overloaded with different return types: not typed",
                    key(fname)
                );
                continue;
            }
            let mut args: Vec<String> = overloads.iter().map(|f| cx.args(name, f)).collect();
            args.sort();
            args.dedup();
            let _ = writeln!(
                fns,
                "        {}: {{ Args: {}; Returns: {} }};",
                key(fname),
                args.join(" | "),
                returns[0]
            );
            let entry = cx.function_runtime(name, overloads);
            let _ = writeln!(fns_rt, "        {}: {},", rt_key(fname), entry.ts());
            rt_functions.insert(fname.clone(), entry.json());
        }
        if fns.is_empty() {
            db.push_str("      functions: {};\n");
        } else {
            let _ = write!(db, "      functions: {{\n{fns}      }};\n");
        }
        if fns_rt.is_empty() {
            rt.push_str("      functions: {},\n");
        } else {
            let _ = write!(rt, "      functions: {{\n{fns_rt}      }},\n");
        }
        db.push_str("    };\n");
        rt.push_str("    },\n");
        let mut rt_part = Map::new();
        rt_part.insert("tables".into(), Value::Object(rt_tables));
        rt_part.insert("functions".into(), Value::Object(rt_functions));
        rt_schemas.insert(name.clone(), Value::Object(rt_part));
    }
    let mut rt_json = Map::new();
    rt_json.insert(
        "defaultSchema".into(),
        Value::from(schema.default_schema.clone()),
    );
    rt_json.insert("schemas".into(), Value::Object(rt_schemas));

    Ok(Rendered {
        db,
        rt,
        rt_json: Value::Object(rt_json),
        dflt: quote(&schema.default_schema),
        app_users: schema.app_users.as_ref().map(|a| {
            format!(
                "{{ schema: {}; table: {} }}",
                quote(&a.schema),
                quote(&a.table)
            )
        }),
        version: schema.version.clone(),
        uses_int8: cx.uses_int8,
        uses_json: cx.uses_json,
        uses_storage_ref: cx.uses_storage_ref,
    })
}

/// Which databases a module covers: `gen types`' `--database` and `--all-databases`, and
/// `GET /data/v1/types`' `database` and `all_databases`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    /// The key's main database (no `database`).
    Main,
    /// One named database, written as `Database`.
    One(String),
    /// Main, then every other database main's schema lists (`databases`, main first).
    All,
}

/// `database` and `all_databases` together: refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictingOptions;

impl std::fmt::Display for ConflictingOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("`database` and `all_databases` can't be combined")
    }
}

impl std::error::Error for ConflictingOptions {}

impl Which {
    /// From `database` and `all_databases` (both set is refused).
    pub fn from_options(
        database: Option<String>,
        all_databases: bool,
    ) -> Result<Which, ConflictingOptions> {
        match (database, all_databases) {
            (Some(_), true) => Err(ConflictingOptions),
            (None, true) => Ok(Which::All),
            (Some(d), false) => Ok(Which::One(d)),
            (None, false) => Ok(Which::Main),
        }
    }

    /// The `database` it fetches first (`None`: the key's main database).
    pub fn database(&self) -> Option<&str> {
        match self {
            Which::One(d) => Some(d),
            Which::Main | Which::All => None,
        }
    }
}

/// What [`render`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Ts,
    Json,
}

impl std::str::FromStr for Format {
    type Err = String;

    /// `ts` or `json`, exactly.
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "ts" => Ok(Format::Ts),
            "json" => Ok(Format::Json),
            other => Err(format!("format {} isn't `ts` or `json`", quote(other))),
        }
    }
}

/// `gen types`' fetch plan, run through `fetch` (`None`: the key's main database, else the
/// named one): the schemas [`render`] takes, named, in order.
///
/// - [`Which::Main`]: main, named `"main"`.
/// - [`Which::One`]: that database, under its name.
/// - [`Which::All`]: main (named `"main"`), then each other database main's `databases`
///   lists (the list names main first, under its real name: skipped), one call at a time.
///
/// The first failed call stops the plan with its error.
pub async fn collect<E, Fut>(
    which: &Which,
    mut fetch: impl FnMut(Option<String>) -> Fut,
) -> Result<Vec<(String, Schema)>, E>
where
    Fut: std::future::Future<Output = Result<Schema, E>>,
{
    match which {
        Which::Main => Ok(vec![("main".to_string(), fetch(None).await?)]),
        Which::One(name) => Ok(vec![(name.clone(), fetch(Some(name.clone())).await?)]),
        Which::All => {
            let main = fetch(None).await?;
            let others: Vec<String> = main.databases.iter().skip(1).cloned().collect();
            let mut all = vec![("main".to_string(), main)];
            for d in others {
                let s = fetch(Some(d.clone())).await?;
                all.push((d, s));
            }
            Ok(all)
        }
    }
}

/// The runtime `types` map for one database: what [`generate`]'s `types` constant holds,
/// as JSON (`{ defaultSchema, schemas }`, a `TypeMap`).
pub fn runtime(schema: &Schema) -> Result<Value> {
    Ok(render_one(schema)?.rt_json)
}

/// The runtime `types` map for several databases, keyed by name: what [`generate_all`]'s
/// `types` constant holds, as JSON.
pub fn runtime_all(all: &[(String, Schema)]) -> Result<Value> {
    distinct(all)?;
    let mut out = Map::new();
    for (n, s) in all {
        let r = render_one(s).map_err(|e| Error::Database {
            database: n.clone(),
            source: Box::new(e),
        })?;
        out.insert(n.clone(), r.rt_json);
    }
    Ok(Value::Object(out))
}

/// What `gen types` writes for [`collect`]'s schemas: one schema is [`generate`]'s module
/// (or [`runtime`]'s map), several are [`generate_all`]'s (or [`runtime_all`]'s). JSON is
/// compact. No schema at all is [`Error::NoSchemas`].
pub fn render(schemas: &[(String, Schema)], format: Format) -> Result<String> {
    match (schemas, format) {
        ([], _) => Err(Error::NoSchemas),
        ([(_, one)], Format::Ts) => generate(one),
        (all, Format::Ts) => generate_all(all),
        ([(_, one)], Format::Json) => Ok(runtime(one)?.to_string()),
        (all, Format::Json) => Ok(runtime_all(all)?.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCHEMA: &str = include_str!("../../../packages/client/fixtures/gen/schema.json");
    const GOLDEN: &str = include_str!("../../../packages/client/fixtures/gen/db.ts");

    fn from(json: &str) -> String {
        generate(&serde_json::from_str(json).unwrap()).unwrap()
    }

    /// The golden file is the SDK's typecheck input too: a changed generator must update it.
    /// `CHELABASE_BLESS=1 cargo test -p chelabase-typegen gen_types_golden` rewrites it.
    #[test]
    fn gen_types_golden() {
        let out = from(SCHEMA);
        if std::env::var_os("CHELABASE_BLESS").is_some() {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packages/client/fixtures/gen/db.ts"
            );
            std::fs::write(path, &out).unwrap();
        }
        assert_eq!(out, GOLDEN);
        assert_eq!(from(SCHEMA), out, "deterministic");
    }

    /// A branch with an app users table marks it on `Database`; one without leaves it out.
    #[test]
    fn gen_marks_app_users_table() {
        let plain = from(SCHEMA);
        assert!(!plain.contains("appUsers"));
        let mut value: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
        value["app_users"] = serde_json::json!({"schema": "public", "table": "users"});
        let out = from(&value.to_string());
        let marked = "  schemas: {\n";
        assert!(out.contains(marked));
        assert!(
            out.contains("  };\n  appUsers: { schema: \"public\"; table: \"users\" };\n}"),
            "{out}"
        );
        // Everything else is unchanged.
        assert_eq!(
            out.replace(
                "  appUsers: { schema: \"public\"; table: \"users\" };\n",
                ""
            ),
            plain
        );
        // Several databases: the mark sits in that database's entry only.
        let main: Schema = serde_json::from_str(&value.to_string()).unwrap();
        let other: Schema = serde_json::from_str(SCHEMA).unwrap();
        let multi = generate_all(&[("main".into(), main), ("other".into(), other)]).unwrap();
        assert_eq!(multi.matches("appUsers:").count(), 1, "{multi}");
        assert!(multi.contains("    appUsers: { schema: \"public\"; table: \"users\" };\n  };"));
    }

    #[test]
    fn nullable_default_generated_rules() {
        let out = from(SCHEMA);
        // orders.slug: generated, nullable, default: in Row only.
        let orders = &out[out.find("\n        orders: {").unwrap()..];
        let (row, rest) = orders.split_once("Insert: {").unwrap();
        let (insert, rest) = rest.split_once("Update: {").unwrap();
        let (update, _) = rest.split_once("\n        };").unwrap();
        assert!(row.contains("slug: string | null;"));
        assert!(!insert.contains("slug") && !update.contains("slug"));
        // nullable without a default: optional and `| null` on insert.
        assert!(insert.contains("ratio?: number | null;"));
        // default, not nullable: optional on insert.
        assert!(insert.contains("qty?: number;"));
        // required: no default, not nullable.
        assert!(insert.contains("total: string;") && row.contains("total: string;"));
        // update: everything optional.
        assert!(update.contains("total?: string;") && update.contains("vip?: boolean;"));
        assert!(
            !update
                .lines()
                .any(|l| l.contains(": ") && !l.contains("?:") && !l.contains("Update"))
        );
    }

    /// A function returning one row (not a set) is typed as that row, not an array, and its
    /// runtime entry carries the row's exact columns.
    #[test]
    fn a_single_row_function_returns_one_row() {
        let out = from(SCHEMA);
        let line = out
            .lines()
            .find(|l| l.trim_start().starts_with("latest_order: { Args"))
            .expect("latest_order is typed");
        let returns = line.split_once("Returns: ").unwrap().1;
        assert!(
            returns.starts_with("{ ") && returns.ends_with(" };"),
            "{line}"
        );
        assert!(
            returns.contains("total: string") && !returns.contains("[] };"),
            "{line}"
        );
        assert!(out.contains("latest_order: { columns: {"), "{out}");
    }

    /// Views, materialized views and foreign tables generate like tables.
    #[test]
    fn every_kind_generates_row_insert_update() {
        let out = from(SCHEMA);
        for (name, column) in [
            ("order_stats", "orders"),
            ("remote_orders", "id"),
            ("my_orders", "id"),
            ("order_totals", "total"),
        ] {
            let at = out
                .find(&format!("        {name}: {{\n          Row: {{"))
                .unwrap_or_else(|| panic!("{name} has a Row\n{out}"));
            let block = &out[at..];
            let block = &block[..block.find("\n        };").unwrap()];
            for part in ["Insert: {", "Update: {", &format!("{column}?: ")] {
                assert!(block.contains(part), "{name}: {part}\n{block}");
            }
        }
    }

    /// A tiny schema around `public`'s `tables` and `functions` JSON.
    fn tiny(version: &str, schema: &str, tables: &str, functions: &str) -> String {
        format!(
            r#"{{"version":{},"default_schema":"public","schemas":{{{}:{{"enums":{{}},"tables":{tables},"functions":{functions}}}}}}}"#,
            quote(version),
            quote(schema)
        )
    }

    fn col(ty: &str) -> String {
        format!(
            r#"{{"type":"{ty}","array":false,"nullable":false,"has_default":false,"generated":false}}"#
        )
    }

    /// U+2028 and U+2029 end a line in JS: escaped in strings, and never inside a comment.
    #[test]
    fn line_separators_are_escaped() {
        assert_eq!(quote("a\u{2028}b\u{2029}c"), "\"a\\u2028b\\u2029c\"");
        let json = tiny(
            "v1\u{2028}evil\nx",
            "public",
            &format!(r#"{{"t\u2028x":{{"columns":{{"c":{}}}}}}}"#, col("text")),
            r#"{"f\u2029":[{"args":[],"returns":{"type":"text","array":false,"set":false}},
                          {"args":[{"name":"a","type":"int4","array":false,"has_default":false}],"returns":{"type":"int4","array":false,"set":false}}]}"#,
        );
        let out = generate(&serde_json::from_str(&json).unwrap()).unwrap();
        assert!(
            !out.contains('\u{2028}') && !out.contains('\u{2029}'),
            "{out}"
        );
        assert!(out.starts_with("// Generated by Chelabase from the data API schema. Don't edit; download it again when the schema changes.\n// Schema version: v1\\u2028evil\\nx\nimport"), "{out}");
        assert!(out.contains(r#"// "f\u2029" is overloaded"#), "{out}");
    }

    /// `__proto__` in an object literal sets the prototype: the runtime map uses a computed key.
    #[test]
    fn proto_keys_are_own_properties_in_the_runtime_map() {
        let json = tiny(
            "v1",
            "__proto__",
            &format!(
                r#"{{"__proto__":{{"columns":{{"__proto__":{}}}}}}}"#,
                col("int8")
            ),
            r#"{"__proto__":[{"args":[],"returns":{"type":"int8","array":false,"set":false}}]}"#,
        );
        let out = generate(&serde_json::from_str(&json).unwrap()).unwrap();
        let runtime = &out[out.find("export const types").unwrap()..];
        assert!(
            !runtime.contains("__proto__:") && !runtime.contains(r#""__proto__":"#),
            "{runtime}"
        );
        assert!(runtime.contains(r#"["__proto__"]: {"#), "{runtime}");
        assert!(
            runtime.contains(r#"["__proto__"]: { ["__proto__"]: "int8" },"#),
            "{runtime}"
        );
        assert!(
            runtime.contains(r#"["__proto__"]: { returns: "int8" },"#),
            "{runtime}"
        );
    }

    /// #102 AC 1: a `storage.object_ref` column (the schema names it `object_ref`) is a
    /// `StorageRef`, `| null` when nullable, an array of them when it's an array; the module
    /// imports `StorageRef` only when it uses it, and the runtime map doesn't list it.
    #[test]
    fn object_ref_columns_are_storage_refs() {
        let nullable = col("object_ref").replace(r#""nullable":false"#, r#""nullable":true"#);
        let array = col("object_ref").replace(r#""array":false"#, r#""array":true"#);
        let json = tiny(
            "v1",
            "public",
            &format!(
                r#"{{"profiles":{{"columns":{{"avatar":{nullable},"banner":{},"gallery":{array}}}}}}}"#,
                col("object_ref")
            ),
            "{}",
        );
        let out = generate(&serde_json::from_str(&json).unwrap()).unwrap();
        assert!(
            out.contains("import type { StorageRef, TypeMap } from \"@chelabase/client\";"),
            "{out}"
        );
        let row = &out[out.find("Row: {").unwrap()..out.find("Insert: {").unwrap()];
        assert!(row.contains("avatar: StorageRef | null;"), "{row}");
        assert!(row.contains("banner: StorageRef;"), "{row}");
        assert!(row.contains("gallery: StorageRef[];"), "{row}");
        let insert = &out[out.find("Insert: {").unwrap()..out.find("Update: {").unwrap()];
        assert!(insert.contains("avatar?: StorageRef | null;"), "{insert}");
        assert!(insert.contains("banner: StorageRef;"), "{insert}");
        assert!(
            out.contains("profiles: {},"),
            "not in the runtime map: {out}"
        );
        // A module without one doesn't import it.
        let plain = tiny(
            "v1",
            "public",
            &format!(r#"{{"t":{{"columns":{{"c":{}}}}}}}"#, col("text")),
            "{}",
        );
        let plain = generate(&serde_json::from_str(&plain).unwrap()).unwrap();
        assert!(!plain.contains("StorageRef"), "{plain}");
    }

    /// A schema name with a dot can't be addressed (names are "schema.name"): refused.
    #[test]
    fn a_dotted_schema_name_is_refused() {
        let json = tiny(
            "v1",
            "my.app",
            &format!(r#"{{"t":{{"columns":{{"c":{}}}}}}}"#, col("text")),
            "{}",
        );
        let err = generate(&serde_json::from_str(&json).unwrap())
            .unwrap_err()
            .to_string();
        assert!(err.contains("\"my.app\"") && err.contains("dot"), "{err}");
    }

    /// Overloads typed alike (text and numeric are both strings) merge their runtime
    /// entries, whichever overload comes first.
    #[test]
    fn same_return_overloads_merge_their_runtime_entries() {
        let tables = format!(
            r#"{{"a":{{"columns":{{"x":{}}}}},"b":{{"columns":{{"x":{}}}}}}}"#,
            col("text"),
            col("numeric")
        );
        let ret = |ty: &str, arg: &str| {
            format!(
                r#"{{"args":[{{"name":"{arg}","type":"int4","array":false,"has_default":false}}],"returns":{{"type":"{ty}","array":false,"set":false}}}}"#
            )
        };
        let functions = format!(
            r#"{{"rows":[{},{}],"scalar":[{},{}]}}"#,
            ret("a", "p"),
            ret("b", "q"),
            ret("text", "p"),
            ret("numeric", "q")
        );
        let out =
            generate(&serde_json::from_str(&tiny("v1", "public", &tables, &functions)).unwrap())
                .unwrap();
        assert!(
            out.contains(r#"rows: { columns: { x: "numeric" } },"#),
            "{out}"
        );
        assert!(out.contains(r#"scalar: { returns: "numeric" },"#), "{out}");
    }

    const ANALYTICS: &str =
        include_str!("../../../packages/client/fixtures/gen/analytics.schema.json");
    const MULTI_GOLDEN: &str = include_str!("../../../packages/client/fixtures/gen/multi.ts");

    /// Two databases generate `Databases` (a key per database) and a `types` map keyed alike,
    /// with `Database` kept as the main one.
    #[test]
    fn gen_types_golden_for_two_databases() {
        let main: Schema = serde_json::from_str(SCHEMA).unwrap();
        let analytics: Schema = serde_json::from_str(ANALYTICS).unwrap();
        let out = generate_all(&[("main".into(), main), ("analytics".into(), analytics)]).unwrap();
        if std::env::var_os("CHELABASE_BLESS").is_some() {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packages/client/fixtures/gen/multi.ts"
            );
            std::fs::write(path, &out).unwrap();
        }
        assert_eq!(out, MULTI_GOLDEN);
        assert!(out.contains("export type Database = Databases[\"main\"];"));
    }

    /// The `types` constant of a generated module, read as JSON: bare keys quoted,
    /// `["k"]` computed keys unwrapped, trailing commas dropped. Strings are JSON strings.
    fn types_constant(module: &str) -> serde_json::Value {
        let start = module.find("export const types = ").unwrap() + "export const types = ".len();
        let end = module.rfind(" as const satisfies").unwrap();
        let src = &module[start..end];
        let mut out = String::new();
        let mut chars = src.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    out.push('"');
                    let mut escaped = false;
                    for c in chars.by_ref() {
                        out.push(c);
                        if escaped {
                            escaped = false;
                        } else if c == '\\' {
                            escaped = true;
                        } else if c == '"' {
                            break;
                        }
                    }
                }
                '[' | ']' => {} // `["__proto__"]`: the string inside stays
                c if c.is_ascii_alphabetic() || c == '_' || c == '$' => {
                    let mut ident = String::from(c);
                    while let Some(&n) = chars.peek() {
                        if n.is_ascii_alphanumeric() || n == '_' || n == '$' {
                            ident.push(n);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    out.push_str(&quote(&ident));
                }
                ',' => {
                    // A trailing comma: the next non-space is a closing brace.
                    let rest: String = chars.clone().collect();
                    if !rest.trim_start().starts_with('}') {
                        out.push(',');
                    }
                }
                c => out.push(c),
            }
        }
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}\n{out}"))
    }

    /// The same JSON with every object's keys written in reverse order.
    fn reversed(v: &serde_json::Value) -> String {
        match v {
            serde_json::Value::Object(m) => {
                let parts: Vec<String> = m
                    .iter()
                    .rev()
                    .map(|(k, v)| format!("{}:{}", quote(k), reversed(v)))
                    .collect();
                format!("{{{}}}", parts.join(","))
            }
            serde_json::Value::Array(a) => {
                format!("[{}]", a.iter().map(reversed).collect::<Vec<_>>().join(","))
            }
            other => other.to_string(),
        }
    }

    fn two() -> Vec<(String, Schema)> {
        vec![
            ("main".into(), serde_json::from_str(SCHEMA).unwrap()),
            ("analytics".into(), serde_json::from_str(ANALYTICS).unwrap()),
        ]
    }

    #[test]
    fn which_follows_the_cli_options() {
        assert_eq!(Which::from_options(None, false), Ok(Which::Main));
        assert_eq!(
            Which::from_options(Some("a".into()), false),
            Ok(Which::One("a".into()))
        );
        assert_eq!(Which::from_options(None, true), Ok(Which::All));
        assert_eq!(
            Which::from_options(Some("a".into()), true),
            Err(ConflictingOptions)
        );
    }

    #[test]
    fn format_parses_ts_and_json_only() {
        assert_eq!("ts".parse::<Format>(), Ok(Format::Ts));
        assert_eq!("json".parse::<Format>(), Ok(Format::Json));
        for bad in ["", "TS", "js", "typescript"] {
            assert!(bad.parse::<Format>().is_err(), "{bad}");
        }
    }

    /// The same schema, whatever its keys' order, always renders the same bytes.
    #[test]
    fn output_is_deterministic() {
        for json in [SCHEMA, ANALYTICS] {
            let value: serde_json::Value = serde_json::from_str(json).unwrap();
            let flipped = reversed(&value);
            assert_ne!(flipped, value.to_string());
            for format in [Format::Ts, Format::Json] {
                let a = render(
                    &[("main".into(), serde_json::from_str(json).unwrap())],
                    format,
                )
                .unwrap();
                let b = render(
                    &[("main".into(), serde_json::from_str(&flipped).unwrap())],
                    format,
                )
                .unwrap();
                let c = render(
                    &[("main".into(), serde_json::from_str(json).unwrap())],
                    format,
                )
                .unwrap();
                assert_eq!(a, b, "{format:?}");
                assert_eq!(a, c, "{format:?}");
            }
        }
        assert_eq!(
            render(&two(), Format::Ts).unwrap(),
            render(&two(), Format::Ts).unwrap()
        );
        assert_eq!(
            render(&two(), Format::Json).unwrap(),
            render(&two(), Format::Json).unwrap()
        );
    }

    /// `format=json` is exactly the module's `types` constant, for one database and several,
    /// including keys a JS object literal treats specially.
    #[test]
    fn the_runtime_json_is_the_modules_types_constant() {
        let one: Schema = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(
            runtime(&one).unwrap(),
            types_constant(&generate(&one).unwrap())
        );
        assert_eq!(
            runtime_all(&two()).unwrap(),
            types_constant(&generate_all(&two()).unwrap())
        );
        assert_eq!(types_constant(GOLDEN), runtime(&one).unwrap());
        assert_eq!(types_constant(MULTI_GOLDEN), runtime_all(&two()).unwrap());
        let proto = tiny(
            "v1",
            "__proto__",
            &format!(
                r#"{{"__proto__":{{"columns":{{"__proto__":{}}}}}}}"#,
                col("int8")
            ),
            r#"{"__proto__":[{"args":[],"returns":{"type":"int8","array":false,"set":false}}],
                "mixed":[{"args":[],"returns":{"type":"int8","array":false,"set":false}},
                         {"args":[{"name":"a","type":"int4","array":false,"has_default":false}],"returns":{"type":"bool","array":false,"set":false}}]}"#,
        );
        let s: Schema = serde_json::from_str(&proto).unwrap();
        let rt = runtime(&s).unwrap();
        assert_eq!(rt, types_constant(&generate(&s).unwrap()));
        assert_eq!(
            rt["schemas"]["__proto__"]["tables"]["__proto__"]["__proto__"],
            "int8"
        );
        // An overload set left untyped is left out of the runtime map too.
        assert!(
            rt["schemas"]["__proto__"]["functions"]
                .get("mixed")
                .is_none(),
            "{rt}"
        );
        // The default schema is there even with nothing in it.
        assert_eq!(rt["defaultSchema"], "public");
        assert_eq!(
            rt["schemas"]["public"],
            serde_json::json!({ "tables": {}, "functions": {} })
        );
    }

    /// `render` picks the module `gen types` writes: one database is `generate`, several
    /// are `generate_all`; JSON is the matching runtime map. Nothing at all is refused.
    #[test]
    fn render_is_the_cli_module() {
        let one: Schema = serde_json::from_str(SCHEMA).unwrap();
        let single = vec![(
            "analytics".to_string(),
            serde_json::from_str::<Schema>(SCHEMA).unwrap(),
        )];
        assert_eq!(render(&single, Format::Ts).unwrap(), GOLDEN);
        assert_eq!(
            render(&single, Format::Ts).unwrap(),
            generate(&one).unwrap()
        );
        assert_eq!(render(&two(), Format::Ts).unwrap(), MULTI_GOLDEN);
        assert_eq!(
            render(&single, Format::Json).unwrap(),
            serde_json::to_string(&runtime(&one).unwrap()).unwrap()
        );
        assert_eq!(
            render(&two(), Format::Json).unwrap(),
            serde_json::to_string(&runtime_all(&two()).unwrap()).unwrap()
        );
        assert!(render(&[], Format::Ts).is_err());
        assert!(render(&[], Format::Json).is_err());
    }

    /// #29: with every database, main is keyed `main`; another database named `main` would
    /// take the same key (a duplicate in `Databases`, a lost entry in `types` and the JSON).
    #[tokio::test]
    async fn all_databases_refuses_a_database_named_main() {
        let listed = SCHEMA.replacen('{', "{\"databases\":[\"app_main\",\"main\"],", 1);
        let schemas = collect(&Which::All, |d: Option<String>| {
            let r = serde_json::from_str::<Schema>(if d.is_none() { &listed } else { ANALYTICS })
                .map_err(|e| e.to_string());
            async move { r }
        })
        .await
        .unwrap();
        for format in [Format::Ts, Format::Json] {
            let err = render(&schemas, format).unwrap_err();
            assert_eq!(
                err,
                Error::DuplicateDatabase {
                    database: "main".into()
                },
                "{format:?}"
            );
            let text = err.to_string();
            assert!(
                text.contains("\"main\"") && text.contains("database=main"),
                "{text}"
            );
        }
        assert!(generate_all(&schemas).is_err());
        assert!(runtime_all(&schemas).is_err());
        // Main's real name is never fetched twice, so `app_main` alone is fine.
        assert!(render(&two(), Format::Ts).is_ok());
    }

    /// A dotted schema in a second database names the database, then the schema.
    #[test]
    fn errors_name_the_database_then_the_schema() {
        let dotted: Schema = serde_json::from_str(&tiny("v1", "my.app", "{}", "{}")).unwrap();
        let all = vec![
            ("main".to_string(), serde_json::from_str(SCHEMA).unwrap()),
            ("other".to_string(), dotted),
        ];
        for err in [
            render(&all, Format::Ts).unwrap_err(),
            render(&all, Format::Json).unwrap_err(),
        ] {
            assert_eq!(err.to_string(), "database \"other\"");
            let source = std::error::Error::source(&err)
                .expect("the schema's error")
                .to_string();
            assert!(
                source.contains("\"my.app\"") && source.contains("dot"),
                "{source}"
            );
        }
    }

    /// `collect` is `gen types`' fetch plan: main, one database, or main then the others
    /// main's schema lists (main's own name skipped, kept as "main").
    #[tokio::test]
    async fn collect_fetches_what_gen_types_fetches() {
        let listed = SCHEMA.replacen('{', "{\"databases\":[\"app_main\",\"analytics\"],", 1);
        let plan = async |which: Which, main: &str| {
            let mut calls: Vec<Option<String>> = Vec::new();
            let got = collect(&which, |d: Option<String>| {
                let r = serde_json::from_str::<Schema>(if d.is_none() { main } else { ANALYTICS })
                    .map_err(|e| e.to_string());
                calls.push(d);
                async move { r }
            })
            .await
            .unwrap();
            (calls, got.into_iter().map(|(n, _)| n).collect::<Vec<_>>())
        };
        assert_eq!(
            plan(Which::Main, &listed).await,
            (vec![None], vec!["main".to_string()])
        );
        assert_eq!(
            plan(Which::One("analytics".into()), &listed).await,
            (
                vec![Some("analytics".to_string())],
                vec!["analytics".to_string()]
            )
        );
        assert_eq!(
            plan(Which::All, &listed).await,
            (
                vec![None, Some("analytics".to_string())],
                vec!["main".to_string(), "analytics".to_string()]
            )
        );
        assert_eq!(
            plan(Which::All, SCHEMA).await,
            (vec![None], vec!["main".to_string()])
        );
        // A failed fetch stops the plan with its error.
        let mut n = 0;
        let err = collect(&Which::All, |_d: Option<String>| {
            n += 1;
            let r = if n == 1 {
                serde_json::from_str::<Schema>(&listed).map_err(|e| e.to_string())
            } else {
                Err("refused".to_string())
            };
            async move { r }
        })
        .await
        .unwrap_err();
        assert_eq!((err.as_str(), n), ("refused", 2));
    }
}
