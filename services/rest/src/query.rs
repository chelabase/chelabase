//! The JSON query language: a strict tree (`"v": 1`, unknown fields rejected anywhere)
//! parsed into [`Op`]. Values stay [`serde_json::Value`] until binding; the planner
//! addresses them with a [`Path`].

use std::fmt;

use serde::Deserialize;
use serde::de::{self, Deserializer};
use serde_json::{Map, Value};

/// Longest alias in bytes: Postgres's identifier limit (NAMEDATALEN - 1).
pub const MAX_ALIAS_BYTES: usize = 63;

/// Most operations one batch may hold.
pub const MAX_BATCH: usize = 50;

#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Select(Select),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    Call(Call),
    /// Never nested; at most [`MAX_BATCH`] operations.
    Batch(Vec<Op>),
}

/// A table or function name as the request spelt it: `"t"` (`schema: None`, meaning the
/// database's `default_schema`) or `"s.t"`. At most one dot, and both parts non-empty.
/// Matched exactly against the catalog (no case folding); never written into SQL.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Name {
    pub schema: Option<String>,
    pub name: String,
}

impl TryFrom<String> for Name {
    type Error = String;

    fn try_from(s: String) -> Result<Self, String> {
        let bad = || format!("bad name {s:?}: use \"name\" or \"schema.name\"");
        let mut parts = s.split('.');
        let first = parts.next().unwrap_or_default();
        let second = parts.next();
        if parts.next().is_some() || first.is_empty() || second.is_some_and(str::is_empty) {
            return Err(bad());
        }
        Ok(match second {
            None => Name {
                schema: None,
                name: first.to_owned(),
            },
            Some(name) => Name {
                schema: Some(first.to_owned()),
                name: name.to_owned(),
            },
        })
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.schema {
            Some(schema) => write!(f, "{schema}.{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Select {
    pub from: Name,
    /// `None` = every column, in catalog order. Never empty.
    #[serde(default)]
    pub select: Option<Vec<SelectItem>>,
    #[serde(rename = "where", default)]
    pub filter: Option<Filter>,
    #[serde(rename = "orderBy", default)]
    pub order_by: Vec<OrderBy>,
    /// ≥ 0; clamped to `max_rows` at binding.
    #[serde(default)]
    pub limit: Option<i64>,
    /// ≥ 0.
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Col(String),
    Aliased { col: String, alias: String },
}

impl<'de> Deserialize<'de> for SelectItem {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Aliased {
            col: String,
            #[serde(rename = "as")]
            alias: String,
        }
        match Value::deserialize(d)? {
            Value::String(col) => Ok(SelectItem::Col(col)),
            v @ Value::Object(_) => {
                let a = Aliased::deserialize(v).map_err(de::Error::custom)?;
                // Postgres truncates identifiers to 63 bytes (two aliases could collide
                // after the duplicate check), rejects `""` and can't hold a NUL.
                if a.alias.is_empty() || a.alias.len() > MAX_ALIAS_BYTES || a.alias.contains('\0') {
                    return Err(de::Error::custom(format!(
                        "alias {:?} must be 1 to {MAX_ALIAS_BYTES} bytes without NUL",
                        a.alias
                    )));
                }
                Ok(SelectItem::Aliased {
                    col: a.col,
                    alias: a.alias,
                })
            }
            _ => Err(de::Error::custom(
                "a select item is a column name or {\"col\", \"as\"}",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderBy {
    pub col: String,
    /// `None` = ascending.
    #[serde(default)]
    pub dir: Option<Dir>,
    /// `None` = Postgres's default for the direction.
    #[serde(default)]
    pub nulls: Option<Nulls>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Nulls {
    First,
    Last,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Insert {
    pub into: Name,
    /// Always a non-empty array of non-empty objects that share one key set.
    pub values: Value,
    #[serde(default)]
    pub returning: Option<Returning>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub table: Name,
    /// Always a non-empty object.
    pub set: Value,
    /// Present unless `all` is true.
    #[serde(rename = "where", default)]
    pub filter: Option<Filter>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub returning: Option<Returning>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delete {
    pub from: Name,
    /// Present unless `all` is true.
    #[serde(rename = "where", default)]
    pub filter: Option<Filter>,
    #[serde(default)]
    pub all: bool,
    #[serde(default)]
    pub returning: Option<Returning>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    #[serde(rename = "fn")]
    pub function: Name,
    /// Named arguments only.
    #[serde(default)]
    pub args: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Returning {
    /// `"*"`: every column, in catalog order.
    All,
    /// Never empty.
    Columns(Vec<String>),
}

impl<'de> Deserialize<'de> for Returning {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Value::deserialize(d)? {
            Value::String(s) if s == "*" => Ok(Returning::All),
            v @ Value::Array(_) => {
                let cols = Vec::<String>::deserialize(v).map_err(de::Error::custom)?;
                if cols.is_empty() {
                    return Err(de::Error::custom("returning needs at least one column"));
                }
                Ok(Returning::Columns(cols))
            }
            _ => Err(de::Error::custom(
                "returning is \"*\" or a list of column names",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Never empty.
    And(Vec<Filter>),
    /// Never empty.
    Or(Vec<Filter>),
    Not(Box<Filter>),
    /// `value` fits `op`: an array for `In`; null, true or false for `Is`; a string for
    /// `Like`/`Ilike`; anything but null for the comparisons.
    Cond {
        col: String,
        op: CondOp,
        value: Value,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CondOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    Is,
    Like,
    Ilike,
}

impl<'de> Deserialize<'de> for Filter {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        filter_from(Value::deserialize(d)?).map_err(de::Error::custom)
    }
}

fn filter_from(v: Value) -> Result<Filter, String> {
    let Value::Object(mut m) = v else {
        return Err("a filter is an object".into());
    };
    for combinator in ["and", "or", "not"] {
        let Some(inner) = m.remove(combinator) else {
            continue;
        };
        if let Some(other) = m.keys().next() {
            return Err(format!(
                "a filter has exactly one operator: `{combinator}` can't sit beside `{other}`"
            ));
        }
        if combinator == "not" {
            return Ok(Filter::Not(Box::new(filter_from(inner)?)));
        }
        let Value::Array(items) = inner else {
            return Err(format!("`{combinator}` takes a list of filters"));
        };
        if items.is_empty() {
            return Err(format!("`{combinator}` needs at least one filter"));
        }
        let items = items
            .into_iter()
            .map(filter_from)
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(if combinator == "and" {
            Filter::And(items)
        } else {
            Filter::Or(items)
        });
    }
    let col = match m.remove("col") {
        Some(Value::String(col)) => col,
        Some(_) => return Err("`col` is a column name".into()),
        None => {
            return Err(
                "a filter is {\"col\", <operator>} or one of \"and\", \"or\", \"not\"".into(),
            );
        }
    };
    if m.len() != 1 {
        return Err(format!(
            "a filter on `{col}` has exactly one operator (eq, neq, lt, lte, gt, gte, in, is, like, ilike)"
        ));
    }
    let (key, value) = m.into_iter().next().expect("one entry");
    let op = match key.as_str() {
        "eq" => CondOp::Eq,
        "neq" => CondOp::Neq,
        "lt" => CondOp::Lt,
        "lte" => CondOp::Lte,
        "gt" => CondOp::Gt,
        "gte" => CondOp::Gte,
        "in" => CondOp::In,
        "is" => CondOp::Is,
        "like" => CondOp::Like,
        "ilike" => CondOp::Ilike,
        other => {
            return Err(format!(
                "unknown field `{other}` in a filter: expected one of eq, neq, lt, lte, gt, gte, in, is, like, ilike"
            ));
        }
    };
    let fits = match op {
        CondOp::In => value.is_array(),
        CondOp::Is => matches!(value, Value::Null | Value::Bool(_)),
        CondOp::Like | CondOp::Ilike => value.is_string(),
        _ => !value.is_null(),
    };
    if !fits {
        return Err(match op {
            CondOp::In => format!("`in` on `{col}` takes an array"),
            CondOp::Is => format!("`is` on `{col}` takes null, true or false"),
            CondOp::Like | CondOp::Ilike => format!("`{key}` on `{col}` takes a string pattern"),
            _ => format!("`{key}` on `{col}` can't compare with null: use \"is\": null"),
        });
    }
    Ok(Filter::Cond { col, op, value })
}

/// The address of one value inside an [`Op`]: what a [`crate::sql::Bind`] points at.
///
/// Contract (relied on by the executor and the shape cache): paths depend only on the
/// tree's shape, so a plan made for one tree resolves correctly against any tree with
/// the same [`crate::shape::shape_hash`]. [`resolve`] turns a path back into the value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Path {
    /// The operation's index inside a batch; `None` when the tree isn't a batch.
    pub op: Option<usize>,
    pub slot: Slot,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Slot {
    /// The `value` of the `Cond` reached from the op's `where` by these child indices:
    /// an index into `and`/`or`, and 0 to step into `not`. Empty = the root is the `Cond`.
    Where(Vec<usize>),
    /// An insert's whole `values` array.
    Rows,
    /// An update's whole `set` object.
    Set,
    /// A call's argument by name.
    Arg(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct QueryError {
    pub message: String,
}

impl QueryError {
    fn new(message: impl Into<String>) -> Self {
        QueryError {
            message: message.into(),
        }
    }

    /// Always `invalid_query` (400).
    pub fn code(&self) -> &'static str {
        "invalid_query"
    }
}

impl From<serde_json::Error> for QueryError {
    fn from(e: serde_json::Error) -> Self {
        QueryError::new(e.to_string())
    }
}

/// Parses a request body. `v` must be 1 at the top; an op inside a batch may repeat
/// `"v": 1` or leave it out.
pub fn parse(body: &[u8]) -> Result<Op, QueryError> {
    let tree: Value = serde_json::from_slice(body)?;
    parse_op(tree, false)
}

fn parse_op(tree: Value, in_batch: bool) -> Result<Op, QueryError> {
    let Value::Object(mut obj) = tree else {
        return Err(QueryError::new("the query is a JSON object"));
    };
    match obj.remove("v") {
        Some(Value::Number(n)) if n.as_u64() == Some(1) => {}
        None if in_batch => {}
        _ => return Err(QueryError::new("\"v\" is required and must be 1")),
    }
    if obj.get("op").and_then(Value::as_str) == Some("batch") {
        if in_batch {
            return Err(QueryError::new("batches don't nest"));
        }
        if let Some(other) = obj.keys().find(|k| *k != "op" && *k != "ops") {
            return Err(QueryError::new(format!(
                "unknown field `{other}` in a batch: expected `ops`"
            )));
        }
        let Some(Value::Array(ops)) = obj.remove("ops") else {
            return Err(QueryError::new("a batch needs `ops`, a list of operations"));
        };
        if ops.len() > MAX_BATCH {
            return Err(QueryError::new(format!(
                "a batch holds at most {MAX_BATCH} operations, got {}",
                ops.len()
            )));
        }
        let ops = ops
            .into_iter()
            .enumerate()
            .map(|(i, op)| {
                parse_op(op, true).map_err(|e| QueryError::new(format!("ops[{i}]: {e}")))
            })
            .collect::<Result<_, _>>()?;
        return Ok(Op::Batch(ops));
    }
    // Dispatched by hand rather than with an internally tagged enum: serde buffers a tagged
    // enum's fields, and the buffer can't hold a number wider than 64 bits.
    let kind = match obj.remove("op") {
        Some(Value::String(kind)) => kind,
        Some(_) => return Err(QueryError::new("\"op\" is a string")),
        None => return Err(QueryError::new("missing field `op`")),
    };
    let fields = Value::Object(obj);
    let op = match kind.as_str() {
        "select" => Op::Select(serde_json::from_value(fields)?),
        "insert" => Op::Insert(serde_json::from_value(fields)?),
        "update" => Op::Update(serde_json::from_value(fields)?),
        "delete" => Op::Delete(serde_json::from_value(fields)?),
        "call" => Op::Call(serde_json::from_value(fields)?),
        other => {
            return Err(QueryError::new(format!(
                "unknown variant `{other}`, expected one of `select`, `insert`, `update`, `delete`, `call`, `batch`"
            )));
        }
    };
    validate(&op)?;
    Ok(op)
}

fn validate(op: &Op) -> Result<(), QueryError> {
    let need_where = |filter: &Option<Filter>, all: bool, verb: &str| {
        if filter.is_none() && !all {
            return Err(QueryError::new(format!(
                "{verb} without \"where\" needs \"all\": true"
            )));
        }
        Ok(())
    };
    match op {
        Op::Select(s) => {
            if s.select.as_ref().is_some_and(Vec::is_empty) {
                return Err(QueryError::new(
                    "\"select\" needs at least one column; leave it out for all columns",
                ));
            }
            if s.limit.is_some_and(|n| n < 0) {
                return Err(QueryError::new("\"limit\" can't be negative"));
            }
            if s.offset.is_some_and(|n| n < 0) {
                return Err(QueryError::new("\"offset\" can't be negative"));
            }
        }
        Op::Insert(i) => {
            let rows = match &i.values {
                Value::Array(rows) if !rows.is_empty() => rows,
                _ => {
                    return Err(QueryError::new(
                        "\"values\" is a non-empty list of row objects",
                    ));
                }
            };
            let mut first: Option<Vec<&String>> = None;
            for row in rows {
                let Value::Object(row) = row else {
                    return Err(QueryError::new("each row in \"values\" is an object"));
                };
                if row.is_empty() {
                    return Err(QueryError::new(
                        "a row in \"values\" needs at least one column",
                    ));
                }
                let mut keys: Vec<&String> = row.keys().collect();
                keys.sort();
                match &first {
                    None => first = Some(keys),
                    Some(f) if *f == keys => {}
                    Some(_) => {
                        return Err(QueryError::new(
                            "all rows in one insert must have the same keys",
                        ));
                    }
                }
            }
        }
        Op::Update(u) => {
            if !u.set.as_object().is_some_and(|s| !s.is_empty()) {
                return Err(QueryError::new("\"set\" is a non-empty object"));
            }
            need_where(&u.filter, u.all, "update")?;
        }
        Op::Delete(d) => need_where(&d.filter, d.all, "delete")?,
        Op::Call(_) | Op::Batch(_) => {}
    }
    Ok(())
}

/// The operation a path's `op` index selects: `op` itself for `None`, or the batch member.
pub(crate) fn sub_op(op: &Op, index: Option<usize>) -> Option<&Op> {
    match (op, index) {
        (Op::Batch(_), None) => None,
        (Op::Batch(ops), Some(i)) => ops.get(i),
        (op, None) => Some(op),
        (_, Some(_)) => None,
    }
}

/// The value `path` addresses in `op`, or `None` when the tree has no such value (only
/// possible for a path made from a tree of a different shape).
pub fn resolve<'a>(op: &'a Op, path: &Path) -> Option<&'a Value> {
    let op = sub_op(op, path.op)?;
    match (&path.slot, op) {
        (Slot::Where(indices), _) => {
            let mut filter = match op {
                Op::Select(s) => s.filter.as_ref(),
                Op::Update(u) => u.filter.as_ref(),
                Op::Delete(d) => d.filter.as_ref(),
                _ => None,
            }?;
            for &i in indices {
                filter = match filter {
                    Filter::And(fs) | Filter::Or(fs) => fs.get(i)?,
                    Filter::Not(f) if i == 0 => f,
                    _ => return None,
                };
            }
            match filter {
                Filter::Cond { value, .. } => Some(value),
                _ => None,
            }
        }
        (Slot::Rows, Op::Insert(i)) => Some(&i.values),
        (Slot::Set, Op::Update(u)) => Some(&u.set),
        (Slot::Arg(name), Op::Call(c)) => c.args.get(name),
        _ => None,
    }
}
