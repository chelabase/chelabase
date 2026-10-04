//! SQL generation. The text depends only on the tree's shape and the catalog: every
//! identifier comes from the catalog's spelling (an alias from the request is the one
//! exception) and is quoted with `"` doubled, and every value is a bind parameter.
//!
//! Each [`Plan`] is one statement that returns exactly one row with one `text` column
//! holding the operation's JSON result.

use std::borrow::Cow;
use std::collections::HashSet;

use serde_json::Value;

use crate::query::{
    CondOp, Dir, Filter, Name, Nulls, Op, OrderBy, Path, Returning, SelectItem, Slot, resolve,
    sub_op,
};

/// The planner's resolved view of a database, as one role sees it.
pub trait Catalog {
    /// The table or view, or `None` when it doesn't exist or the role can't reach it.
    /// The planner always passes a qualified name (`schema` is `Some`).
    fn table(&self, name: &Name) -> Option<&TableInfo>;
    /// The one function with this qualified name that accepts exactly these argument
    /// names (each is a parameter; every parameter without a default is among them).
    /// `arg_names` is sorted.
    fn function(&self, name: &Name, arg_names: &[&str]) -> Result<&FunctionInfo, FnLookupError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    /// In catalog (attnum) order.
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    /// `format_type` output, e.g. `numeric(10,2)`, `public.mood`, `text[]`.
    pub type_sql: String,
    pub array: bool,
    /// `like`/`ilike` are allowed (text, varchar, char, citext and the like).
    pub text_like: bool,
    pub generated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionInfo {
    pub schema: String,
    pub name: String,
    /// `(name, type_sql)` in declaration order.
    pub args: Vec<(String, String)>,
    pub returns_set: bool,
    /// Returns `void`: the call answers JSON `null` (`to_json` of a void is `""`).
    pub returns_void: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FnLookupError {
    /// No such function, not reachable by the role, or no overload takes these names.
    NotFound,
    /// More than one overload takes these names.
    Ambiguous,
}

/// One statement: `binds[i]` is `$i+1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub sql: String,
    pub binds: Vec<Bind>,
}

/// How to fill one parameter. Plans hold no values, so one plan serves every tree with
/// the same shape; [`resolve_bind`] reads the values from the request's tree.
///
/// - `Text(path)`: a `text` parameter (the SQL casts it): a JSON string binds its
///   contents, null binds SQL NULL, and a number, boolean, array or object binds its
///   compact JSON text.
/// - `Json(path)`: a `jsonb` parameter holding the value as is (null is JSON `null`).
/// - `Limit`: an `int8`: the op's `limit` clamped to the `max_rows` passed to
///   [`resolve_bind`], or `max_rows` when it has none (writes never have one, so their
///   `returning` cap is `max_rows`). The setting is read at binding, never frozen into
///   a plan.
/// - `Offset`: an `int8`: the op's `offset`, or 0.
///
/// `op` is the batch index, as in [`Path::op`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    Text(Path),
    Json(Path),
    Limit { op: Option<usize> },
    Offset { op: Option<usize> },
}

/// A parameter's value, ready to bind: `Text` as `text` (`None` = NULL), `Json` as
/// `jsonb`, `Int` as `int8`.
#[derive(Debug, Clone, PartialEq)]
pub enum BindValue<'a> {
    Text(Option<Cow<'a, str>>),
    Json(&'a Value),
    Int(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("unknown table {0}")]
    UnknownTable(String),
    #[error("unknown column {column} in {table}")]
    UnknownColumn { table: String, column: String },
    #[error("unknown function {0}")]
    UnknownFunction(String),
    #[error("{0}")]
    InvalidQuery(String),
}

impl PlanError {
    /// The API error code (unknown_table and unknown_function are 404, the others 400).
    pub fn code(&self) -> &'static str {
        match self {
            PlanError::UnknownTable(_) => "unknown_table",
            PlanError::UnknownColumn { .. } => "unknown_column",
            PlanError::UnknownFunction(_) => "unknown_function",
            PlanError::InvalidQuery(_) => "invalid_query",
        }
    }
}

/// Plans `op`: one [`Plan`] per operation (a batch gives one per member, in order).
/// `default_schema` qualifies bare names, so it belongs in any cache key next to the
/// shape hash; `max_rows` is applied later, by [`resolve_bind`].
pub fn plan(op: &Op, catalog: &impl Catalog, default_schema: &str) -> Result<Vec<Plan>, PlanError> {
    let one = |op: &Op, index: Option<usize>| {
        let mut p = Planner {
            catalog,
            index,
            default_schema,
            binds: Vec::new(),
        };
        let sql = p.op(op)?;
        Ok(Plan {
            sql,
            binds: p.binds,
        })
    };
    match op {
        Op::Batch(ops) => ops
            .iter()
            .enumerate()
            .map(|(i, op)| one(op, Some(i)))
            .collect(),
        op => Ok(vec![one(op, None)?]),
    }
}

/// The value for one parameter of a plan made from a tree with `op`'s shape, with the
/// database's current `max_rows`. `None` only when the plan came from a tree of a
/// different shape.
pub fn resolve_bind<'a>(op: &'a Op, bind: &Bind, max_rows: i64) -> Option<BindValue<'a>> {
    Some(match bind {
        Bind::Text(path) => BindValue::Text(match resolve(op, path)? {
            Value::Null => None,
            Value::String(s) => Some(Cow::Borrowed(s.as_str())),
            other => Some(Cow::Owned(other.to_string())),
        }),
        Bind::Json(path) => BindValue::Json(resolve(op, path)?),
        Bind::Limit { op: index } => {
            let limit = match sub_op(op, *index)? {
                Op::Select(s) => s.limit,
                _ => None,
            };
            BindValue::Int(limit.map_or(max_rows, |l| l.min(max_rows)))
        }
        Bind::Offset { op: index } => match sub_op(op, *index)? {
            Op::Select(s) => BindValue::Int(s.offset.unwrap_or(0)),
            _ => return None,
        },
    })
}

/// `"name"` with any `"` doubled.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The type a filter value or argument is cast to. Type modifiers that would silently
/// round or truncate the value (and change what matches) are dropped: `numeric(p,s)` →
/// `numeric`, `character varying(n)` → `character varying`, `character(n)` → `bpchar`.
fn cast_type(type_sql: &str) -> Cow<'_, str> {
    let base = type_sql.trim_end_matches("[]");
    let suffix = &type_sql[base.len()..];
    for (prefix, plain) in [
        ("numeric(", "numeric"),
        ("character varying(", "character varying"),
        ("character(", "bpchar"),
    ] {
        if let Some(rest) = base.strip_prefix(prefix)
            && let Some(modifier) = rest.strip_suffix(')')
            && !modifier.is_empty()
            && modifier.bytes().all(|b| b.is_ascii_digit() || b == b',')
        {
            return Cow::Owned(format!("{plain}{suffix}"));
        }
    }
    Cow::Borrowed(type_sql)
}

struct Planner<'c, C> {
    catalog: &'c C,
    index: Option<usize>,
    default_schema: &'c str,
    binds: Vec<Bind>,
}

fn table_sql(t: &TableInfo) -> String {
    format!("{}.{}", quote_ident(&t.schema), quote_ident(&t.name))
}

fn all_columns(t: &TableInfo) -> String {
    t.columns
        .iter()
        .map(|c| quote_ident(&c.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn invalid(message: impl Into<String>) -> PlanError {
    PlanError::InvalidQuery(message.into())
}

impl<'c, C: Catalog> Planner<'c, C> {
    fn bind(&mut self, bind: Bind) -> String {
        self.binds.push(bind);
        format!("${}", self.binds.len())
    }

    fn path(&self, slot: Slot) -> Path {
        Path {
            op: self.index,
            slot,
        }
    }

    fn qualify(&self, name: &Name) -> Name {
        Name {
            schema: Some(
                name.schema
                    .clone()
                    .unwrap_or_else(|| self.default_schema.to_owned()),
            ),
            name: name.name.clone(),
        }
    }

    fn table(&self, name: &Name) -> Result<&'c TableInfo, PlanError> {
        let qualified = self.qualify(name);
        self.catalog
            .table(&qualified)
            .ok_or_else(|| PlanError::UnknownTable(qualified.to_string()))
    }

    fn column<'t>(&self, table: &'t TableInfo, name: &str) -> Result<&'t ColumnInfo, PlanError> {
        table
            .columns
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| PlanError::UnknownColumn {
                table: format!("{}.{}", table.schema, table.name),
                column: name.to_owned(),
            })
    }

    fn op(&mut self, op: &Op) -> Result<String, PlanError> {
        match op {
            Op::Select(s) => self.select(s),
            Op::Insert(i) => {
                let table = self.table(&i.into)?;
                let keys = i.values[0]
                    .as_object()
                    .map(|r| r.keys())
                    .into_iter()
                    .flatten();
                let cols = self.write_columns(table, keys)?;
                let rows = self.bind(Bind::Json(self.path(Slot::Rows)));
                let t = table_sql(table);
                let stmt = format!(
                    "INSERT INTO {t} ({cols}) SELECT {cols} FROM jsonb_populate_recordset(NULL::{t}, {rows})"
                );
                self.wrap_write(table, stmt, i.returning.as_ref())
            }
            Op::Update(u) => {
                let table = self.table(&u.table)?;
                let keys = u.set.as_object().map(|s| s.keys()).into_iter().flatten();
                let cols = self.write_columns(table, keys)?;
                let set = self.bind(Bind::Json(self.path(Slot::Set)));
                let t = table_sql(table);
                let filter = self.where_clause(table, u.filter.as_ref())?;
                let stmt = format!(
                    "UPDATE {t} SET ({cols}) = (SELECT {cols} FROM jsonb_populate_record(NULL::{t}, {set})){filter}"
                );
                self.wrap_write(table, stmt, u.returning.as_ref())
            }
            Op::Delete(d) => {
                let table = self.table(&d.from)?;
                let filter = self.where_clause(table, d.filter.as_ref())?;
                let stmt = format!("DELETE FROM {}{filter}", table_sql(table));
                self.wrap_write(table, stmt, d.returning.as_ref())
            }
            Op::Call(c) => {
                let qualified = self.qualify(&c.function);
                let mut names: Vec<&str> = c.args.keys().map(String::as_str).collect();
                names.sort_unstable();
                let unknown = || PlanError::UnknownFunction(qualified.to_string());
                let f = match self.catalog.function(&qualified, &names) {
                    Ok(f) => f,
                    Err(FnLookupError::NotFound) => return Err(unknown()),
                    Err(FnLookupError::Ambiguous) => {
                        return Err(invalid(format!(
                            "more than one function {qualified} takes the arguments ({}); \
                             the call can't pick one",
                            names.join(", ")
                        )));
                    }
                };
                if !names.iter().all(|n| f.args.iter().any(|(a, _)| a == n)) {
                    return Err(unknown());
                }
                let mut args = Vec::new();
                for (name, type_sql) in &f.args {
                    if c.args.contains_key(name) {
                        let slot = self.path(Slot::Arg(name.clone()));
                        let ty = cast_type(type_sql);
                        // Like columns: an array takes a JSON array, json/jsonb the JSON
                        // value itself; everything else is text cast to the parameter type.
                        let value = if type_sql.ends_with("[]") {
                            let p = self.bind(Bind::Json(slot));
                            format!("CAST(ARRAY(SELECT jsonb_array_elements_text({p})) AS {ty})")
                        } else if type_sql == "jsonb" || type_sql == "json" {
                            let p = self.bind(Bind::Json(slot));
                            format!("CAST({p} AS {ty})")
                        } else {
                            let p = self.bind(Bind::Text(slot));
                            format!("CAST({p} AS {ty})")
                        };
                        args.push(format!("{} => {value}", quote_ident(name)));
                    }
                }
                let call = format!(
                    "{}.{}({})",
                    quote_ident(&f.schema),
                    quote_ident(&f.name),
                    args.join(", ")
                );
                Ok(if f.returns_set {
                    // `x.f` is the result column whatever the row type holds: a scalar
                    // stays a scalar, and a row type's own column `f` can't shadow it.
                    format!(
                        "SELECT coalesce(json_agg(x.f), '[]')::text FROM (SELECT {call} AS f) x"
                    )
                } else if f.returns_void {
                    // In the target list, so a STABLE or IMMUTABLE function still runs.
                    format!("SELECT NULLIF({call}::text, '')")
                } else {
                    format!("SELECT to_json({call})::text")
                })
            }
            Op::Batch(_) => Err(invalid("batches don't nest")),
        }
    }

    fn select(&mut self, s: &crate::query::Select) -> Result<String, PlanError> {
        let table = self.table(&s.from)?;
        let cols = match &s.select {
            None => all_columns(table),
            Some(items) => {
                let mut out = Vec::new();
                let mut names = HashSet::new();
                for item in items {
                    let (col, alias) = match item {
                        SelectItem::Col(c) => (c, None),
                        SelectItem::Aliased { col, alias } => (col, Some(alias)),
                    };
                    let column = self.column(table, col)?;
                    let output = alias.unwrap_or(&column.name);
                    if !names.insert(output.as_str()) {
                        return Err(invalid(format!(
                            "\"select\" names `{output}` twice; give one an alias"
                        )));
                    }
                    out.push(match alias {
                        None => quote_ident(&column.name),
                        Some(a) => format!("{} AS {}", quote_ident(&column.name), quote_ident(a)),
                    });
                }
                out.join(", ")
            }
        };
        let filter = self.where_clause(table, s.filter.as_ref())?;
        let order = self.order_by(table, &s.order_by)?;
        let limit = self.bind(Bind::Limit { op: self.index });
        let offset = self.bind(Bind::Offset { op: self.index });
        Ok(format!(
            "SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT {cols} FROM {}{filter}{order} LIMIT {limit} OFFSET {offset}) t",
            table_sql(table)
        ))
    }

    fn order_by(&self, table: &TableInfo, order: &[OrderBy]) -> Result<String, PlanError> {
        if order.is_empty() {
            return Ok(String::new());
        }
        let mut terms = Vec::new();
        for o in order {
            let column = self.column(table, &o.col)?;
            let dir = match o.dir.unwrap_or(Dir::Asc) {
                Dir::Asc => "ASC",
                Dir::Desc => "DESC",
            };
            let nulls = match o.nulls {
                None => "",
                Some(Nulls::First) => " NULLS FIRST",
                Some(Nulls::Last) => " NULLS LAST",
            };
            terms.push(format!("{} {dir}{nulls}", quote_ident(&column.name)));
        }
        Ok(format!(" ORDER BY {}", terms.join(", ")))
    }

    /// The written columns, in catalog order, as a quoted list.
    fn write_columns<'k>(
        &self,
        table: &TableInfo,
        keys: impl Iterator<Item = &'k String>,
    ) -> Result<String, PlanError> {
        let mut positions = Vec::new();
        for key in keys {
            let column = self.column(table, key)?;
            if column.generated {
                return Err(invalid(format!(
                    "column `{}` is generated and can't be written",
                    column.name
                )));
            }
            positions.push(
                table
                    .columns
                    .iter()
                    .position(|c| std::ptr::eq(c, column))
                    .expect("column of this table"),
            );
        }
        positions.sort_unstable();
        Ok(positions
            .into_iter()
            .map(|i| quote_ident(&table.columns[i].name))
            .collect::<Vec<_>>()
            .join(", "))
    }

    fn wrap_write(
        &mut self,
        table: &TableInfo,
        stmt: String,
        returning: Option<&Returning>,
    ) -> Result<String, PlanError> {
        let cols = match returning {
            None => return Ok(format!("WITH r AS ({stmt}) SELECT '[]'::text")),
            Some(Returning::All) => all_columns(table),
            Some(Returning::Columns(cols)) => {
                let mut seen = HashSet::new();
                let mut out = Vec::new();
                for col in cols {
                    let column = self.column(table, col)?;
                    if !seen.insert(column.name.as_str()) {
                        return Err(invalid(format!("\"returning\" names `{col}` twice")));
                    }
                    out.push(quote_ident(&column.name));
                }
                out.join(", ")
            }
        };
        let limit = self.bind(Bind::Limit { op: self.index });
        Ok(format!(
            "WITH r AS ({stmt} RETURNING {cols}) SELECT coalesce(json_agg(t.*), '[]')::text FROM (SELECT * FROM r LIMIT {limit}) t"
        ))
    }

    fn where_clause(
        &mut self,
        table: &TableInfo,
        filter: Option<&Filter>,
    ) -> Result<String, PlanError> {
        match filter {
            None => Ok(String::new()),
            Some(f) => Ok(format!(
                " WHERE {}",
                self.filter(table, f, &mut Vec::new())?
            )),
        }
    }

    fn filter(
        &mut self,
        table: &TableInfo,
        filter: &Filter,
        at: &mut Vec<usize>,
    ) -> Result<String, PlanError> {
        match filter {
            Filter::And(fs) | Filter::Or(fs) => {
                let joiner = if matches!(filter, Filter::And(_)) {
                    " AND "
                } else {
                    " OR "
                };
                let mut parts = Vec::new();
                for (i, f) in fs.iter().enumerate() {
                    at.push(i);
                    parts.push(self.filter(table, f, at)?);
                    at.pop();
                }
                Ok(format!("({})", parts.join(joiner)))
            }
            Filter::Not(f) => {
                at.push(0);
                let inner = self.filter(table, f, at)?;
                at.pop();
                Ok(format!("NOT ({inner})"))
            }
            Filter::Cond { col, op, value } => {
                let column = self.column(table, col)?;
                let c = quote_ident(&column.name);
                let slot = Slot::Where(at.clone());
                let jsonb = column.type_sql == "jsonb" && !column.array;
                let ty = cast_type(&column.type_sql);
                let cmp = match op {
                    CondOp::Eq => "=",
                    CondOp::Neq => "<>",
                    CondOp::Lt => "<",
                    CondOp::Lte => "<=",
                    CondOp::Gt => ">",
                    CondOp::Gte => ">=",
                    CondOp::Is => {
                        return match value {
                            Value::Null => Ok(format!("{c} IS NULL")),
                            Value::Bool(b) => {
                                if column.type_sql != "boolean" {
                                    return Err(invalid(format!(
                                        "`is` {b} needs a boolean column; `{}` is {}",
                                        column.name, column.type_sql
                                    )));
                                }
                                Ok(format!("{c} IS {}", if *b { "TRUE" } else { "FALSE" }))
                            }
                            _ => Err(invalid("`is` takes null, true or false")),
                        };
                    }
                    CondOp::Like | CondOp::Ilike => {
                        if !column.text_like || column.array {
                            return Err(invalid(format!(
                                "like and ilike need a text column; `{}` is {}",
                                column.name, column.type_sql
                            )));
                        }
                        let p = self.bind(Bind::Text(self.path(slot)));
                        let kw = if *op == CondOp::Like { "LIKE" } else { "ILIKE" };
                        return Ok(format!("{c} {kw} CAST({p} AS text)"));
                    }
                    CondOp::In => {
                        if column.array {
                            return Err(invalid(format!(
                                "`in` isn't supported on the array column `{}`",
                                column.name
                            )));
                        }
                        let p = self.bind(Bind::Json(self.path(slot)));
                        return Ok(if jsonb {
                            format!("{c} = ANY (SELECT x FROM jsonb_array_elements({p}) x)")
                        } else {
                            format!(
                                "{c} = ANY (SELECT CAST(x AS {ty}) FROM jsonb_array_elements_text({p}) x)"
                            )
                        });
                    }
                };
                Ok(if column.array {
                    let p = self.bind(Bind::Json(self.path(slot)));
                    format!("{c} {cmp} CAST(ARRAY(SELECT jsonb_array_elements_text({p})) AS {ty})")
                } else if jsonb {
                    let p = self.bind(Bind::Json(self.path(slot)));
                    format!("{c} {cmp} CAST({p} AS jsonb)")
                } else {
                    let p = self.bind(Bind::Text(self.path(slot)));
                    format!("{c} {cmp} CAST({p} AS {ty})")
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::cast_type;

    #[test]
    fn cast_type_drops_only_value_changing_modifiers() {
        for (from, to) in [
            ("numeric(10,2)", "numeric"),
            ("numeric(5)", "numeric"),
            ("character varying(3)", "character varying"),
            ("character varying(3)[]", "character varying[]"),
            ("character(2)", "bpchar"),
            ("character(2)[][]", "bpchar[][]"),
            ("timestamp(3) with time zone", "timestamp(3) with time zone"),
            ("bit(3)", "bit(3)"),
            ("public.mood", "public.mood"),
            ("\"numeric(1)\"", "\"numeric(1)\""),
            ("numeric(x)", "numeric(x)"),
        ] {
            assert_eq!(cast_type(from), to, "{from}");
        }
    }
}
