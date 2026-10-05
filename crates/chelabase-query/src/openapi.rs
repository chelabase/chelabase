//! The request grammar as OpenAPI schemas (the `openapi` feature), for rest's
//! `openapi/data.json`.
//!
//! [`crate::parse`] dispatches by hand over a [`serde_json::Value`], so there is no request
//! type to derive from: these marker types describe what it accepts, and
//! `tests/openapi_fidelity.rs` runs the same inputs through both. JSON Schema can't express
//! five of the parser's rules, which the server alone enforces: an insert's rows share one
//! key set; an alias and `database` are at most 63 *bytes* (`maxLength` counts characters);
//! `v` and `limit` must be written as integers (`1.0` is rejected); `limit` and `offset` fit
//! an `i64` (9223372036854775807).

use std::borrow::Cow;

use utoipa::openapi::schema::{
    AdditionalProperties, AllOfBuilder, AnyOfBuilder, ArrayBuilder, Discriminator, Object,
    ObjectBuilder, OneOfBuilder, Schema, SchemaType, Type,
};
use utoipa::openapi::{Ref, RefOr};
use utoipa::{PartialSchema, ToSchema};

use crate::query::{MAX_ALIAS_BYTES, MAX_BATCH, MAX_DATABASE_BYTES};

fn closed() -> Option<AdditionalProperties<Schema>> {
    Some(AdditionalProperties::FreeForm(false))
}

fn of<T: ToSchema>() -> Ref {
    Ref::from_schema_name(T::name())
}

fn string() -> ObjectBuilder {
    ObjectBuilder::new().schema_type(Type::String)
}

fn types(types: impl IntoIterator<Item = Type>) -> SchemaType {
    types.into_iter().collect()
}

fn one_value(value: &str) -> ObjectBuilder {
    string().enum_values(Some([value]))
}

/// `schema`, or `null` (which the parser reads as the field left out).
fn or_null(schema: impl Into<RefOr<Schema>>) -> OneOfBuilder {
    OneOfBuilder::new()
        .item(schema)
        .item(ObjectBuilder::new().schema_type(Type::Null))
}

/// `v`: the query language's version, 1.
fn version() -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(Type::Integer)
        .enum_values(Some([1]))
        .description(Some("The query language's version"))
}

/// A marker type whose schema is `$schema`, under its own name.
macro_rules! component {
    ($(#[$doc:meta])* $name:ident => $schema:expr) => {
        $(#[$doc])*
        pub struct $name;

        impl PartialSchema for $name {
            fn schema() -> RefOr<Schema> {
                $schema
            }
        }

        impl ToSchema for $name {
            fn name() -> Cow<'static, str> {
                Cow::Borrowed(stringify!($name))
            }
        }
    };
}

#[derive(Clone, Copy)]
enum OpKind {
    Select,
    Insert,
    Update,
    Delete,
    Call,
}

impl OpKind {
    fn op(self) -> &'static str {
        match self {
            OpKind::Select => "select",
            OpKind::Insert => "insert",
            OpKind::Update => "update",
            OpKind::Delete => "delete",
            OpKind::Call => "call",
        }
    }
}

/// One operation's object: at the top level `v` is required and `database` allowed; in a
/// batch `v` is optional and `database` isn't allowed. Closed either way.
fn op_schema(kind: OpKind, top_level: bool) -> Object {
    let mut o = ObjectBuilder::new()
        .property("v", version())
        .property("op", one_value(kind.op()));
    if top_level {
        o = o.required("v");
        o = o.property(
            "database",
            string()
                .min_length(Some(1))
                .max_length(Some(MAX_DATABASE_BYTES))
                .description(Some(
                    "The Postgres database to run in (1 to 63 bytes); the key's main database when left out",
                )),
        );
    }
    o = o.required("op");
    let returning = || or_null(of::<Returning>());
    let where_ = || {
        or_null(of::<Filter>())
            .description(Some("The rows to change; required unless `all` is true"))
    };
    let all = || {
        ObjectBuilder::new()
            .schema_type(Type::Boolean)
            .description(Some("`true` to act on every row without `where`"))
    };
    o = match kind {
        OpKind::Select => o
            .property("from", of::<TableName>())
            .required("from")
            .property(
                "select",
                ArrayBuilder::new()
                    .schema_type(types([Type::Array, Type::Null]))
                    .items(of::<SelectItem>())
                    .min_items(Some(1))
                    .description(Some("The columns to return; every column when left out")),
            )
            .property("where", or_null(of::<Filter>()))
            .property("orderBy", ArrayBuilder::new().items(of::<OrderBy>()))
            .property(
                "limit",
                ObjectBuilder::new()
                    .schema_type(types([Type::Integer, Type::Null]))
                    .minimum(Some(0))
                    .description(Some("At most this many rows (the server caps it)")),
            )
            .property(
                "offset",
                ObjectBuilder::new()
                    .schema_type(types([Type::Integer, Type::Null]))
                    .minimum(Some(0))
                    .description(Some("Skip this many rows first")),
            ),
        OpKind::Insert => o
            .property("into", of::<TableName>())
            .required("into")
            .property(
                "values",
                ArrayBuilder::new()
                    .items(ObjectBuilder::new().min_properties(Some(1)))
                    .min_items(Some(1))
                    .description(Some(
                        "The rows, each an object of column values; every row has the same keys",
                    )),
            )
            .required("values")
            .property("returning", returning()),
        OpKind::Update => o
            .property("table", of::<TableName>())
            .required("table")
            .property(
                "set",
                ObjectBuilder::new()
                    .min_properties(Some(1))
                    .description(Some("The new column values")),
            )
            .required("set")
            .property("where", where_())
            .property("all", all())
            .property("returning", returning()),
        OpKind::Delete => o
            .property("from", of::<TableName>())
            .required("from")
            .property("where", where_())
            .property("all", all())
            .property("returning", returning()),
        OpKind::Call => o.property("fn", of::<TableName>()).required("fn").property(
            "args",
            ObjectBuilder::new().description(Some("Named arguments, each any JSON value")),
        ),
    };
    o.additional_properties(closed()).build()
}

/// The operation as a component: an update or delete also needs a non-null `where` or
/// `"all": true`.
fn op_component(kind: OpKind, top_level: bool) -> RefOr<Schema> {
    let object = op_schema(kind, top_level);
    match kind {
        OpKind::Update | OpKind::Delete => AllOfBuilder::new()
            .item(object)
            .item(
                AnyOfBuilder::new()
                    .item(
                        ObjectBuilder::new()
                            .property("where", ObjectBuilder::new())
                            .required("where"),
                    )
                    .item(
                        ObjectBuilder::new()
                            .property(
                                "all",
                                ObjectBuilder::new()
                                    .schema_type(Type::Boolean)
                                    .enum_values(Some([true])),
                            )
                            .required("all"),
                    ),
            )
            .into(),
        _ => object.into(),
    }
}

component! {
    /// A table, view or function: `"name"` (the database's default schema) or `"schema.name"`.
    TableName => string()
        .pattern(Some(r"^[^.]+(\.[^.]+)?$"))
        .description(Some(
            "`\"name\"` (in the database's default schema) or `\"schema.name\"`, matched exactly",
        ))
        .into()
}

component! {
    /// A column to return: its name, or `{col, as}` to rename it.
    SelectItem => OneOfBuilder::new()
        .item(string().description(Some("A column name")))
        .item(
            ObjectBuilder::new()
                .property("col", string())
                .required("col")
                .property(
                    "as",
                    string()
                        .min_length(Some(1))
                        .max_length(Some(MAX_ALIAS_BYTES))
                        .pattern(Some(r"^[^\x00]+$"))
                        .description(Some("The key in each result row: 1 to 63 bytes, no NUL")),
                )
                .required("as")
                .additional_properties(closed()),
        )
        .into()
}

component! {
    /// One sort key.
    OrderBy => ObjectBuilder::new()
        .property("col", string())
        .required("col")
        .property(
            "dir",
            string()
                .enum_values(Some(["asc", "desc"]))
                .description(Some("Ascending when left out")),
        )
        .property(
            "nulls",
            string()
                .enum_values(Some(["first", "last"]))
                .description(Some("Postgres's default for the direction when left out")),
        )
        .additional_properties(closed())
        .into()
}

component! {
    /// The changed rows' columns to return: `"*"` or a list of names.
    Returning => OneOfBuilder::new()
        .item(one_value("*"))
        .item(ArrayBuilder::new().items(string()).min_items(Some(1)))
        .description(Some(
            "The changed rows' columns to return: `\"*\"` for all, or a list of column names; no rows (`[]`) when left out",
        ))
        .into()
}

component! {
    /// A row filter: `and`, `or`, `not`, or one condition `{col, <operator>: value}`.
    Filter => {
        let list = |name: &str| {
            ObjectBuilder::new()
                .property(
                    name,
                    ArrayBuilder::new().items(of::<Filter>()).min_items(Some(1)),
                )
                .required(name)
                .additional_properties(closed())
        };
        let not_null = || {
            ObjectBuilder::new().schema_type(types([
                Type::String,
                Type::Number,
                Type::Boolean,
                Type::Array,
                Type::Object,
            ]))
        };
        let mut cond = ObjectBuilder::new()
            .property("col", string().description(Some("A column name (any string, `\"and\"` included)")))
            .required("col");
        for op in ["eq", "neq", "lt", "lte", "gt", "gte"] {
            cond = cond.property(op, not_null());
        }
        let cond = cond
            .property("in", ArrayBuilder::new().items(ObjectBuilder::new().schema_type(SchemaType::AnyValue)))
            .property(
                "is",
                ObjectBuilder::new().schema_type(types([Type::Null, Type::Boolean])),
            )
            .property("like", string())
            .property("ilike", string())
            .min_properties(Some(2))
            .max_properties(Some(2))
            .additional_properties(closed())
            .description(Some(
                "One condition: `col` and exactly one operator. Comparisons take any value but null (use `is`); `in` a list; `is` null, true or false; `like`/`ilike` a pattern",
            ));
        OneOfBuilder::new()
            .item(list("and"))
            .item(list("or"))
            .item(
                ObjectBuilder::new()
                    .property("not", of::<Filter>())
                    .required("not")
                    .additional_properties(closed()),
            )
            .item(cond)
            .into()
    }
}

component! {
    /// A top-level select.
    SelectRequest => op_component(OpKind::Select, true)
}

component! {
    /// A top-level insert.
    InsertRequest => op_component(OpKind::Insert, true)
}

component! {
    /// A top-level update.
    UpdateRequest => op_component(OpKind::Update, true)
}

component! {
    /// A top-level delete.
    DeleteRequest => op_component(OpKind::Delete, true)
}

component! {
    /// A top-level function call.
    CallRequest => op_component(OpKind::Call, true)
}

component! {
    /// A select inside a batch.
    BatchSelect => op_component(OpKind::Select, false)
}

component! {
    /// An insert inside a batch.
    BatchInsert => op_component(OpKind::Insert, false)
}

component! {
    /// An update inside a batch.
    BatchUpdate => op_component(OpKind::Update, false)
}

component! {
    /// A delete inside a batch.
    BatchDelete => op_component(OpKind::Delete, false)
}

component! {
    /// A function call inside a batch.
    BatchCall => op_component(OpKind::Call, false)
}

/// `op` → the component, for a discriminator's mapping.
fn mapping(entries: [(&'static str, Cow<'static, str>); 5]) -> Vec<(&'static str, String)> {
    entries
        .into_iter()
        .map(|(op, name)| (op, format!("#/components/schemas/{name}")))
        .collect()
}

component! {
    /// Operations run in order in one transaction; the result is the list of their results.
    BatchRequest => ObjectBuilder::new()
        .property("v", version())
        .required("v")
        .property(
            "database",
            string()
                .min_length(Some(1))
                .max_length(Some(MAX_DATABASE_BYTES))
                .description(Some(
                    "The Postgres database every operation runs in (1 to 63 bytes); the key's main database when left out",
                )),
        )
        .property("op", one_value("batch"))
        .required("op")
        .property(
            "ops",
            ArrayBuilder::new()
                .items(
                    OneOfBuilder::new()
                        .item(of::<BatchSelect>())
                        .item(of::<BatchInsert>())
                        .item(of::<BatchUpdate>())
                        .item(of::<BatchDelete>())
                        .item(of::<BatchCall>())
                        .discriminator(Some(Discriminator::with_mapping(
                            "op",
                            mapping([
                                ("select", BatchSelect::name()),
                                ("insert", BatchInsert::name()),
                                ("update", BatchUpdate::name()),
                                ("delete", BatchDelete::name()),
                                ("call", BatchCall::name()),
                            ]),
                        ))),
                )
                .max_items(Some(MAX_BATCH))
                .description(Some("At most 50 operations; batches don't nest")),
        )
        .required("ops")
        .additional_properties(closed())
        .into()
}

component! {
    /// What a query returns, as Postgres's JSON: an array of row objects for a select and for
    /// writes (`[]` without `returning`); the function's result for a call (an array for a
    /// set-returning function, `null` for a void one); the list of results for a batch.
    QueryResult => ObjectBuilder::new()
        .schema_type(SchemaType::AnyValue)
        .description(Some(
            "Rows as an array of objects for a select and for writes (`[]` without `returning`); the function's result for a call (an array for a set-returning function, `null` for a void one); for a batch, the array of each operation's result in order",
        ))
        .into()
}

/// A data API request: one operation or a batch, discriminated by `op`.
pub struct QueryRequest;

impl PartialSchema for QueryRequest {
    fn schema() -> RefOr<Schema> {
        let batch = format!("#/components/schemas/{}", BatchRequest::name());
        let mut mapping = mapping([
            ("select", SelectRequest::name()),
            ("insert", InsertRequest::name()),
            ("update", UpdateRequest::name()),
            ("delete", DeleteRequest::name()),
            ("call", CallRequest::name()),
        ]);
        mapping.push(("batch", batch));
        OneOfBuilder::new()
            .item(of::<SelectRequest>())
            .item(of::<InsertRequest>())
            .item(of::<UpdateRequest>())
            .item(of::<DeleteRequest>())
            .item(of::<CallRequest>())
            .item(of::<BatchRequest>())
            .discriminator(Some(Discriminator::with_mapping("op", mapping)))
            .description(Some(
                "One operation, or a batch of them, as a JSON tree. Unknown fields are rejected everywhere",
            ))
            .into()
    }
}

impl ToSchema for QueryRequest {
    fn name() -> Cow<'static, str> {
        Cow::Borrowed("QueryRequest")
    }

    /// Every schema the request references, once (`Filter` refers only to itself).
    fn schemas(schemas: &mut Vec<(String, RefOr<Schema>)>) {
        fn push<T: ToSchema>(schemas: &mut Vec<(String, RefOr<Schema>)>) {
            schemas.push((T::name().into_owned(), T::schema()));
        }
        push::<SelectRequest>(schemas);
        push::<InsertRequest>(schemas);
        push::<UpdateRequest>(schemas);
        push::<DeleteRequest>(schemas);
        push::<CallRequest>(schemas);
        push::<BatchRequest>(schemas);
        push::<BatchSelect>(schemas);
        push::<BatchInsert>(schemas);
        push::<BatchUpdate>(schemas);
        push::<BatchDelete>(schemas);
        push::<BatchCall>(schemas);
        push::<Filter>(schemas);
        push::<TableName>(schemas);
        push::<SelectItem>(schemas);
        push::<OrderBy>(schemas);
        push::<Returning>(schemas);
    }
}
