//! `chelabase gen types`: the data API's schema JSON to a TypeScript module.
//!
//! The output is `Database` (the shape `@chelabase/client`'s `DatabaseShape` wants) plus
//! `types`, the runtime map of which columns and returns are `int8` or `numeric`. Keys are
//! sorted, so the same schema always gives the same text.

use anyhow::{Context, Result, bail};
use chelabase_types::data_key::{DataKeyKind, parse_kind};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

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

/// A JSON string is a TS string literal, once U+2028 and U+2029 are escaped (JSON allows
/// them raw; in JS they end a line, and so a comment).
fn quote(s: &str) -> String {
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
    fn function_runtime(&self, schema: &str, overloads: &[Function]) -> String {
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
            return format!("{{ returns: {} }}", quote(kind));
        }
        if columns.is_empty() {
            return "{}".into();
        }
        let cols: Vec<String> = columns
            .iter()
            .map(|(n, k)| format!("{}: {}", rt_key(n), quote(k)))
            .collect();
        format!("{{ columns: {{ {} }} }}", cols.join(", "))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Row,
    Insert,
    Update,
}

/// `{ id: "int8", total: "numeric" }`, or `{}`.
fn exact_columns(t: &Table) -> String {
    let cols: Vec<String> = t
        .columns
        .iter()
        .filter_map(|(n, c)| exact(&c.ty).map(|k| format!("{}: {}", rt_key(n), quote(k))))
        .collect();
    if cols.is_empty() {
        "{}".into()
    } else {
        format!("{{ {} }}", cols.join(", "))
    }
}

/// One database, rendered: the bodies of its `schemas` for the type and the runtime map.
struct Rendered {
    db: String,
    rt: String,
    dflt: String,
    app_users: Option<String>,
    version: String,
    uses_int8: bool,
    uses_json: bool,
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
    imports.push("TypeMap");
    format!(
        "// Generated by `chelabase gen types`. Do not edit.\n// Schema version: {versions}\nimport type {{ {} }} from \"@chelabase/client\";\n\n",
        imports.join(", ")
    )
}

/// Indents every line after the first by two spaces.
fn nest(text: &str) -> String {
    text.replace('\n', "\n  ")
}

/// The module text for one database: `Database` and `types`.
pub fn generate(schema: &Schema) -> Result<String> {
    let r = render(schema)?;
    let mut out = header(&[&r], &comment(&r.version));
    let _ = writeln!(out, "export type Database = {};\n", r.db_type());
    let _ = writeln!(out, "export const types = {} as const satisfies TypeMap;", r.rt_object());
    Ok(out)
}

/// The module text for several databases (main first, as given): `Databases` with a key per
/// database, `Database` as the first one, and `types` keyed alike.
pub fn generate_all(all: &[(String, Schema)]) -> Result<String> {
    let rendered = all
        .iter()
        .map(|(n, s)| render(s).with_context(|| format!("database {}", quote(n))))
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
    let _ = writeln!(out, "export type Database = Databases[{}];\n", quote(&all[0].0));
    out.push_str("export const types = {\n");
    for ((n, _), r) in all.iter().zip(&rendered) {
        let _ = writeln!(out, "  {}: {},", rt_key(n), nest(&r.rt_object()));
    }
    out.push_str("} as const satisfies Record<string, TypeMap>;\n");
    Ok(out)
}

fn render(schema: &Schema) -> Result<Rendered> {
    // A name is "table" or "schema.table", split at its one dot: a schema whose name has a
    // dot can't be addressed.
    if let Some(dotted) = schema.schemas.keys().find(|n| n.contains('.')) {
        bail!(
            "schema {} has a dot in its name, which the data API's names (\"schema.table\") can't address; rename it, or revoke the key's role's USAGE on it",
            quote(dotted)
        );
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
    };

    let mut db = String::new();
    let mut rt = String::new();
    for name in names {
        let p = part(name);
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
                let _ = writeln!(rt, "        {}: {},", rt_key(tn), exact_columns(t));
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
            let _ = writeln!(
                fns_rt,
                "        {}: {},",
                rt_key(fname),
                cx.function_runtime(name, overloads)
            );
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
    }

    Ok(Rendered {
        db,
        rt,
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
    })
}

/// How long the schema call may take.
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Which databases `gen types` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Which {
    /// The key's main database (no `?database=`).
    Main,
    /// One named database (`?database=<name>`), written as `Database`.
    One(String),
    /// Main, then every other database main's schema response lists.
    All,
}

/// `chelabase gen types`: checks the key, fetches the schema (or schemas), writes the module.
pub async fn run(url: &str, key: Option<String>, out: Option<&Path>, which: Which) -> Result<()> {
    fetch_and_write(url, key, out, which, FETCH_TIMEOUT).await
}

async fn fetch_schema(
    http: &reqwest::Client,
    url: &str,
    key: &str,
    database: Option<&str>,
) -> Result<Schema> {
    let endpoint = format!("{}/data/v1/schema", url.trim_end_matches('/'));
    let mut target = reqwest::Url::parse(&endpoint).with_context(|| format!("the URL {endpoint}"))?;
    if let Some(d) = database {
        target.query_pairs_mut().append_pair("database", d);
    }
    let resp = http
        .get(target)
        .bearer_auth(key)
        .send().await.with_context(|| format!("GET {endpoint}"))?;
    let status = resp.status();
    let body = resp.text().await.context("reading the schema response")?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        let which = database.map(|d| format!(" (database {})", quote(d))).unwrap_or_default();
        bail!("GET {endpoint}{which} answered {status}: {snippet}");
    }
    serde_json::from_str(&body)
        .context("the schema response isn't the data API schema (is the server up to date? it must name `default_schema`)")
}

async fn fetch_and_write(
    url: &str,
    key: Option<String>,
    out: Option<&Path>,
    which: Which,
    timeout: std::time::Duration,
) -> Result<()> {
    let key =
        key.context("no data key: pass --key or set CHELABASE_DATA_KEY (a secret key, chk_sec_…)")?;
    if parse_kind(key.trim()) != Some(DataKeyKind::Secret) {
        bail!(
            "gen types needs a secret key (chk_sec_…); a public key, or anything that isn't a data key, can't read the schema"
        );
    }
    let key = key.trim();
    let http = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("an HTTP client")?;
    let text = match which {
        Which::Main => generate(&fetch_schema(&http, url, key, None).await?)?,
        Which::One(name) => generate(&fetch_schema(&http, url, key, Some(&name)).await?)?,
        Which::All => {
            let main = fetch_schema(&http, url, key, None).await?;
            // The list names main first, under its real name: that is the schema just fetched
            // (kept under the key "main"); the rest follow in order.
            let others: Vec<String> = main.databases.iter().skip(1).cloned().collect();
            if others.is_empty() {
                generate(&main)?
            } else {
                let mut all = vec![("main".to_string(), main)];
                for d in others {
                    let s = fetch_schema(&http, url, key, Some(&d)).await?;
                    all.push((d, s));
                }
                generate_all(&all)?
            }
        }
    };
    match out {
        Some(path) => {
            std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?
        }
        None => print!("{text}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type Hits = Arc<Mutex<Vec<(String, String)>>>;

    const SCHEMA: &str = include_str!("../../../../packages/client/fixtures/gen/schema.json");
    const GOLDEN: &str = include_str!("../../../../packages/client/fixtures/gen/db.ts");

    fn from(json: &str) -> String {
        generate(&serde_json::from_str(json).unwrap()).unwrap()
    }

    /// The golden file is the SDK's typecheck input too: a changed generator must update it.
    /// `CHELABASE_BLESS=1 cargo test -p chelabase gen_types_golden` rewrites it.
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
            out.replace("  appUsers: { schema: \"public\"; table: \"users\" };\n", ""),
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
        assert!(out.starts_with("// Generated by `chelabase gen types`. Do not edit.\n// Schema version: v1\\u2028evil\\nx\nimport"), "{out}");
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

    /// The schema call gives up after its timeout.
    #[tokio::test]
    async fn the_schema_call_times_out() {
        assert_eq!(FETCH_TIMEOUT, std::time::Duration::from_secs(30));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        // Accepts and never answers.
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                open.push(socket);
            }
        });
        let key =
            chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key;
        let started = std::time::Instant::now();
        let err = fetch_and_write(&url, Some(key), None, Which::Main, std::time::Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{err:#}"
        );
        held.abort();
    }

    #[tokio::test]
    async fn refuses_public_key() {
        let hits = Hits::default();
        let url = serve(hits.clone()).await;
        let public =
            chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Public).key;
        for bad in [public.as_str(), "nonsense"] {
            let err = run(&url, Some(bad.into()), None, Which::Main)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("secret key"), "{err}");
        }
        let err = run(&url, None, None, Which::Main).await.unwrap_err().to_string();
        assert!(err.contains("CHELABASE_DATA_KEY"), "{err}");
        assert!(
            hits.lock().unwrap().is_empty(),
            "no call before the key is checked"
        );
    }

    #[tokio::test]
    async fn fetches_with_the_secret_key_and_writes_the_file() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let url = serve(hits.clone()).await;
        let key =
            chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&format!("{url}/"), Some(key.clone()), Some(&out), Which::Main)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), GOLDEN);
        let hits = hits.lock().unwrap();
        assert_eq!(
            hits.as_slice(),
            [("/data/v1/schema".to_string(), format!("Bearer {key}"))]
        );
    }

    #[tokio::test]
    async fn a_refused_call_is_an_error_naming_the_status() {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let url = serve_status(hits, 401).await;
        let key =
            chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key;
        let err = run(&url, Some(key), None, Which::Main).await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
    }

    const ANALYTICS: &str =
        include_str!("../../../../packages/client/fixtures/gen/analytics.schema.json");
    const MULTI_GOLDEN: &str = include_str!("../../../../packages/client/fixtures/gen/multi.ts");

    /// Serves the main schema (with a `databases` list) or the analytics one by `?database=`,
    /// recording `path?query` and the bearer.
    async fn serve_multi(hits: Hits) -> String {
        use axum::{Router, extract::State, http::HeaderMap, http::StatusCode, http::Uri, routing::get};
        let app = Router::new()
            .fallback(get(
                move |State(h): State<Hits>, uri: Uri, headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    let pq = uri.path_and_query().unwrap().to_string();
                    h.lock().unwrap().push((pq.clone(), auth.to_string()));
                    match uri.query() {
                        None => (StatusCode::OK, SCHEMA.replacen("{", "{\"databases\":[\"app_main\",\"analytics\"],", 1)),
                        Some("database=analytics") => (StatusCode::OK, ANALYTICS.to_string()),
                        Some(_) => (StatusCode::NOT_FOUND, "{}".to_string()),
                    }
                },
            ))
            .with_state(hits);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn secret() -> String {
        chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key
    }

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

    #[tokio::test]
    async fn database_flag_adds_the_query_string() {
        let hits = Hits::default();
        let url = serve_multi(hits.clone()).await;
        let key = secret();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&url, Some(key.clone()), Some(&out), Which::One("analytics".into()))
            .await
            .unwrap();
        assert_eq!(
            hits.lock().unwrap().as_slice(),
            [("/data/v1/schema?database=analytics".to_string(), format!("Bearer {key}"))]
        );
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.contains("export type Database = {") && text.contains("reports: {"), "{text}");
        assert!(!text.contains("Databases"), "{text}");
    }

    #[tokio::test]
    async fn all_databases_reads_the_list_then_fetches_each() {
        let hits = Hits::default();
        let url = serve_multi(hits.clone()).await;
        let key = secret();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&url, Some(key.clone()), Some(&out), Which::All).await.unwrap();
        let paths: Vec<String> = hits.lock().unwrap().iter().map(|h| h.0.clone()).collect();
        assert_eq!(paths, ["/data/v1/schema", "/data/v1/schema?database=analytics"]);
        // Main's real name (`app_main`) is skipped, not fetched twice; it keeps the key "main".
        let text = std::fs::read_to_string(&out).unwrap();
        assert!(!text.contains("app_main"), "{text}");
        assert_eq!(text, MULTI_GOLDEN);
    }

    #[tokio::test]
    async fn all_databases_with_only_main_writes_the_plain_file() {
        // The default mock has no `databases` field: one database, today's output.
        let hits = Hits::default();
        let url = serve(hits.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&url, Some(secret()), Some(&out), Which::All).await.unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap(), GOLDEN);
        assert_eq!(hits.lock().unwrap().len(), 1);
    }

    async fn serve(hits: Hits) -> String {
        serve_status(hits, 200).await
    }

    async fn serve_status(hits: Hits, status: u16) -> String {
        use axum::{
            Router, extract::State, http::HeaderMap, http::StatusCode, http::Uri, routing::get,
        };
        let app = Router::new()
            .fallback(get(
                move |State(h): State<Hits>, uri: Uri, headers: HeaderMap| async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    h.lock()
                        .unwrap()
                        .push((uri.path().to_string(), auth.to_string()));
                    (StatusCode::from_u16(status).unwrap(), SCHEMA)
                },
            ))
            .with_state(hits);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }
}
