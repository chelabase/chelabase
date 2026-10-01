//! `cheladb gen types`: the data API's schema JSON to a TypeScript module.
//!
//! The output is `Database` (the shape `@cheladb/client`'s `DatabaseShape` wants) plus
//! `types`, the runtime map of which columns and returns are `int8` or `numeric`. Keys are
//! sorted, so the same schema always gives the same text.

use anyhow::{Context, Result, bail};
use cheladb_types::data_key::{DataKeyKind, parse_kind};
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

/// The module text.
pub fn generate(schema: &Schema) -> Result<String> {
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

    let mut imports = Vec::new();
    if cx.uses_int8 {
        imports.push("Int8");
    }
    if cx.uses_json {
        imports.push("Json");
    }
    imports.push("TypeMap");
    let dflt = quote(&schema.default_schema);
    let mut out = String::new();
    let _ = writeln!(out, "// Generated by `cheladb gen types`. Do not edit.");
    let _ = writeln!(out, "// Schema version: {}", comment(&schema.version));
    let _ = writeln!(
        out,
        "import type {{ {} }} from \"@cheladb/client\";\n",
        imports.join(", ")
    );
    let _ = writeln!(
        out,
        "export type Database = {{\n  defaultSchema: {dflt};\n  schemas: {{\n{db}  }};\n}};\n"
    );
    let _ = writeln!(
        out,
        "export const types = {{\n  defaultSchema: {dflt},\n  schemas: {{\n{rt}  }},\n}} as const satisfies TypeMap;"
    );
    Ok(out)
}

/// How long the schema call may take.
pub const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// `cheladb gen types`: checks the key, fetches the schema, writes the module.
pub async fn run(url: &str, key: Option<String>, out: Option<&Path>) -> Result<()> {
    fetch_and_write(url, key, out, FETCH_TIMEOUT).await
}

async fn fetch_and_write(
    url: &str,
    key: Option<String>,
    out: Option<&Path>,
    timeout: std::time::Duration,
) -> Result<()> {
    let key =
        key.context("no data key: pass --key or set CHELADB_DATA_KEY (a secret key, chk_sec_…)")?;
    if parse_kind(key.trim()) != Some(DataKeyKind::Secret) {
        bail!(
            "gen types needs a secret key (chk_sec_…); a public key, or anything that isn't a data key, can't read the schema"
        );
    }
    let key = key.trim();
    let endpoint = format!("{}/data/v1/schema", url.trim_end_matches('/'));
    let resp = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .context("an HTTP client")?
        .get(&endpoint)
        .bearer_auth(key)
        .send()
        .await
        .with_context(|| format!("GET {endpoint}"))?;
    let status = resp.status();
    let body = resp.text().await.context("reading the schema response")?;
    if !status.is_success() {
        let snippet: String = body.chars().take(200).collect();
        bail!("GET {endpoint} answered {status}: {snippet}");
    }
    let schema: Schema = serde_json::from_str(&body)
        .context("the schema response isn't the data API schema (is the server up to date? it must name `default_schema`)")?;
    let text = generate(&schema)?;
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
    /// `CHELADB_BLESS=1 cargo test -p cheladb gen_types_golden` rewrites it.
    #[test]
    fn gen_types_golden() {
        let out = from(SCHEMA);
        if std::env::var_os("CHELADB_BLESS").is_some() {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../packages/client/fixtures/gen/db.ts"
            );
            std::fs::write(path, &out).unwrap();
        }
        assert_eq!(out, GOLDEN);
        assert_eq!(from(SCHEMA), out, "deterministic");
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
        assert!(out.starts_with("// Generated by `cheladb gen types`. Do not edit.\n// Schema version: v1\\u2028evil\\nx\nimport"), "{out}");
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
            cheladb_types::data_key::generate(cheladb_types::data_key::DataKeyKind::Secret).key;
        let started = std::time::Instant::now();
        let err = fetch_and_write(&url, Some(key), None, std::time::Duration::from_millis(200))
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
            cheladb_types::data_key::generate(cheladb_types::data_key::DataKeyKind::Public).key;
        for bad in [public.as_str(), "nonsense"] {
            let err = run(&url, Some(bad.into()), None)
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("secret key"), "{err}");
        }
        let err = run(&url, None, None).await.unwrap_err().to_string();
        assert!(err.contains("CHELADB_DATA_KEY"), "{err}");
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
            cheladb_types::data_key::generate(cheladb_types::data_key::DataKeyKind::Secret).key;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("db.ts");
        run(&format!("{url}/"), Some(key.clone()), Some(&out))
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
            cheladb_types::data_key::generate(cheladb_types::data_key::DataKeyKind::Secret).key;
        let err = run(&url, Some(key), None).await.unwrap_err().to_string();
        assert!(err.contains("401"), "{err}");
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
