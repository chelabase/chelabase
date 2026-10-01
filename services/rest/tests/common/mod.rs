#![allow(dead_code)]

use rest::query::{Name, Op, parse};
use rest::sql::{
    Catalog, ColumnInfo, FnLookupError, FunctionInfo, Plan, PlanError, TableInfo, plan,
};

pub const MAX_ROWS: i64 = 1000;

pub struct FakeCatalog {
    pub tables: Vec<TableInfo>,
    /// Each function with the names of its arguments that have no default.
    pub functions: Vec<(FunctionInfo, Vec<&'static str>)>,
}

fn col(name: &str, type_sql: &str) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        type_sql: type_sql.into(),
        array: false,
        text_like: false,
        generated: false,
    }
}

impl FakeCatalog {
    pub fn new() -> Self {
        let order = TableInfo {
            schema: "public".into(),
            name: "Order".into(),
            columns: vec![
                col("id", "bigint"),
                ColumnInfo {
                    text_like: true,
                    ..col("select", "text")
                },
                col("total", "numeric(10,2)"),
                col("mood", "public.mood"),
                ColumnInfo {
                    array: true,
                    ..col("tags", "text[]")
                },
                col("meta", "jsonb"),
                ColumnInfo {
                    generated: true,
                    ..col("made", "bigint")
                },
            ],
        };
        let invoices = TableInfo {
            schema: "billing".into(),
            name: "invoices".into(),
            columns: vec![
                col("id", "uuid"),
                col("amount", "numeric"),
                col("paid", "boolean"),
                ColumnInfo {
                    text_like: true,
                    ..col("code", "character varying(3)")
                },
            ],
        };
        let add_points = FunctionInfo {
            schema: "public".into(),
            name: "add_points".into(),
            args: vec![
                ("user_id".into(), "uuid".into()),
                ("n".into(), "integer".into()),
            ],
            returns_set: false,
            returns_void: false,
        };
        let recent = FunctionInfo {
            schema: "public".into(),
            name: "recent_orders".into(),
            args: vec![("n".into(), "integer".into())],
            returns_set: true,
            returns_void: false,
        };
        // returns setof (f int, g int): a column named like the result alias
        let pairs = FunctionInfo {
            schema: "public".into(),
            name: "pairs".into(),
            args: vec![],
            returns_set: true,
            returns_void: false,
        };
        // tag(ids int4[], meta jsonb, raw json, codes varchar(3)[]) returns void
        let tag = FunctionInfo {
            schema: "public".into(),
            name: "tag".into(),
            args: vec![
                ("ids".into(), "integer[]".into()),
                ("meta".into(), "jsonb".into()),
                ("raw".into(), "json".into()),
                ("codes".into(), "character varying(3)[]".into()),
            ],
            returns_set: false,
            returns_void: false,
        };
        let touch = FunctionInfo {
            schema: "public".into(),
            name: "touch".into(),
            args: vec![],
            returns_set: false,
            returns_void: true,
        };
        FakeCatalog {
            tables: vec![order, invoices],
            functions: vec![
                (add_points, vec!["user_id"]),
                (recent, vec!["n"]),
                (pairs, vec![]),
                (tag, vec![]),
                (touch, vec![]),
            ],
        }
    }
}

impl Catalog for FakeCatalog {
    fn table(&self, name: &Name) -> Option<&TableInfo> {
        let schema = name.schema.as_deref().expect("plan passes qualified names");
        self.tables
            .iter()
            .find(|t| t.schema == schema && t.name == name.name)
    }

    fn function(&self, name: &Name, arg_names: &[&str]) -> Result<&FunctionInfo, FnLookupError> {
        let schema = name.schema.as_deref().expect("plan passes qualified names");
        let found: Vec<&FunctionInfo> = self
            .functions
            .iter()
            .filter(|(f, required)| {
                f.schema == schema
                    && f.name == name.name
                    && arg_names.iter().all(|a| f.args.iter().any(|(n, _)| n == a))
                    && required.iter().all(|r| arg_names.contains(r))
            })
            .map(|(f, _)| f)
            .collect();
        match found.as_slice() {
            [] => Err(FnLookupError::NotFound),
            [one] => Ok(one),
            _ => Err(FnLookupError::Ambiguous),
        }
    }
}

pub fn op(json: &str) -> Op {
    parse(json.as_bytes()).unwrap_or_else(|e| panic!("{json} should parse: {e}"))
}

pub fn plans(json: &str) -> Result<Vec<Plan>, PlanError> {
    plan(&op(json), &FakeCatalog::new(), "public")
}

pub fn one(json: &str) -> Plan {
    let mut v = plans(json).unwrap_or_else(|e| panic!("{json} should plan: {e}"));
    assert_eq!(v.len(), 1);
    v.remove(0)
}
