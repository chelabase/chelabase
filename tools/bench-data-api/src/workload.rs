//! The benchmark's four operations on `bench_items`, built as request trees and run through
//! `chelabase_query::parse` and the planner, so the SQL and binds are what rest sends.
//! Plans are made once per [`OpKind`] from the `authenticated` role's view; [`Workload::next`]
//! only draws fresh values.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::Context;
use chelabase_query::sql::{Catalog, Plan};
use rand::RngExt;
use serde_json::{Value, json};

/// The role every variant runs as, and whose view the plans come from.
pub const ROLE: &str = "authenticated";
/// Rows seeded into `bench_items` (ids `1..=SEEDED_ROWS`).
pub const SEEDED_ROWS: i64 = 100_000;
/// `n` takes values `0..N_VALUES` (`n = id % N_VALUES`).
pub const N_VALUES: i64 = 1_000;
/// The list op's `limit`.
pub const LIST_LIMIT: i64 = 100;
/// Inserted ids start at `INSERT_ID_BASE + rand(0..INSERT_ID_SPREAD)`, past every seeded id
/// and (with high probability) every earlier run's.
pub const INSERT_ID_BASE: i64 = 1_000_000;
pub const INSERT_ID_SPREAD: i64 = 1_000_000_000;
/// Inserted rows' `n`: outside `0..N_VALUES`, so List100 never sees them.
pub const INSERT_N: i64 = -1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum OpKind {
    ByKey,
    List100,
    Insert,
    Batch,
}

pub const ALL_OPS: [OpKind; 4] = [
    OpKind::ByKey,
    OpKind::List100,
    OpKind::Insert,
    OpKind::Batch,
];

pub struct BenchOp {
    pub kind: OpKind,
    pub op: chelabase_query::Op,
    /// The request body (`op` parsed from it): what R sends to rest's engine.
    pub body: Arc<[u8]>,
    pub plans: std::sync::Arc<Vec<chelabase_query::sql::Plan>>,
}

/// The plans of each [`OpKind`] and the insert id counter.
pub struct Workload {
    plans: HashMap<OpKind, Arc<Vec<Plan>>>,
    next_id: AtomicI64,
}

impl Workload {
    /// Introspects the database as the owner (a tokio-postgres connection to `owner_url`, as
    /// `chelabase_query` takes), takes the `authenticated` role's view of it and plans every op
    /// with `default_schema` for unqualified names.
    pub async fn build(owner_url: &str, default_schema: &str) -> anyhow::Result<Workload> {
        let (owner, connection) = tokio_postgres::connect(owner_url, tokio_postgres::NoTls)
            .await
            .context("connecting as the owner (tokio-postgres)")?;
        let connection = tokio::spawn(connection);
        let version: String = owner
            .query_one("SELECT version::text FROM chelabase.schema_version", &[])
            .await
            .context("reading the schema version")?
            .try_get(0)?;
        let schema = chelabase_query::introspect(&owner, version)
            .await
            .map_err(|e| anyhow::anyhow!("introspecting the database: {e:?}"))?;
        let view = chelabase_query::role_view(&owner, &schema, ROLE)
            .await
            .map_err(|e| anyhow::anyhow!("computing the {ROLE} view: {e:?}"))?;
        drop(owner);
        connection
            .await
            .context("the owner connection's task")?
            .context("the owner connection")?;
        Workload::from_catalog(view.as_ref(), default_schema)
    }

    /// Plans every op against `catalog`.
    pub fn from_catalog(catalog: &impl Catalog, default_schema: &str) -> anyhow::Result<Workload> {
        // A sample tree of each kind gives the shape; its values don't matter.
        let sample_id = AtomicI64::new(INSERT_ID_BASE);
        let mut rng = rand::rng();
        let mut plans = HashMap::new();
        for kind in ALL_OPS {
            let op = parse(&serde_json::to_vec(&tree(kind, &mut rng, &sample_id))?)?;
            let planned = chelabase_query::plan(&op, catalog, default_schema)
                .with_context(|| format!("planning {kind:?}"))?;
            plans.insert(kind, Arc::new(planned));
        }
        Ok(Workload {
            plans,
            next_id: AtomicI64::new(INSERT_ID_BASE + rand::random_range(0..INSERT_ID_SPREAD)),
        })
    }

    /// A fresh op of `kind`: new random values, the same plans.
    pub fn next(&self, kind: OpKind, rng: &mut impl rand::Rng) -> BenchOp {
        let body: Arc<[u8]> = serde_json::to_vec(&tree(kind, rng, &self.next_id))
            .expect("the workload's trees serialize")
            .into();
        let op = parse(&body).expect("the workload's trees parse");
        BenchOp {
            kind,
            op,
            body,
            plans: self.plans[&kind].clone(),
        }
    }
}

fn parse(body: &[u8]) -> anyhow::Result<chelabase_query::Op> {
    Ok(chelabase_query::parse(body)
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", String::from_utf8_lossy(body)))?
        .op)
}

/// The request tree of one op (`v: 1`).
fn tree(kind: OpKind, rng: &mut impl rand::Rng, next_id: &AtomicI64) -> Value {
    match kind {
        OpKind::ByKey => json!({
            "v": 1,
            "op": "select",
            "from": "bench_items",
            "where": {"col": "id", "eq": rng.random_range(1..=SEEDED_ROWS)},
        }),
        OpKind::List100 => json!({
            "v": 1,
            "op": "select",
            "from": "bench_items",
            "where": {"col": "n", "eq": rng.random_range(0..N_VALUES)},
            "orderBy": [{"col": "id"}],
            "limit": LIST_LIMIT,
        }),
        OpKind::Insert => {
            let owner = uuid::Builder::from_random_bytes(rng.random()).into_uuid();
            json!({
                "v": 1,
                "op": "insert",
                "into": "bench_items",
                "values": [{
                    "id": next_id.fetch_add(1, Ordering::Relaxed),
                    "owner": owner,
                    "title": "bench",
                    "n": INSERT_N,
                }],
                "returning": ["id", "owner", "title", "n"],
            })
        }
        OpKind::Batch => {
            let ops: Vec<Value> = [OpKind::ByKey, OpKind::List100, OpKind::Insert]
                .into_iter()
                .map(|k| {
                    let mut t = tree(k, rng, next_id);
                    t.as_object_mut().expect("an object").remove("v");
                    t
                })
                .collect();
            json!({"v": 1, "op": "batch", "ops": ops})
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use chelabase_query::Op;
    use chelabase_query::query::{Filter, Name};
    use chelabase_query::sql::{ColumnInfo, FnLookupError, FunctionInfo, TableInfo};
    use rand::SeedableRng;

    #[test]
    fn bench_ops_carry_their_request_body() {
        let w = Workload::from_catalog(&BenchCatalog::new(), "public").unwrap();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        for kind in ALL_OPS {
            let op = w.next(kind, &mut rng);
            let tree: Value = serde_json::from_slice(&op.body).expect("JSON");
            assert_eq!(tree["v"], 1, "{kind:?}");
            // The body parses to the op the variants run.
            let again = chelabase_query::parse(&op.body).unwrap().op;
            assert_eq!(format!("{again:?}"), format!("{:?}", op.op), "{kind:?}");
        }
    }

    /// `public.bench_items` as the seed creates it.
    pub(crate) struct BenchCatalog(TableInfo);

    impl BenchCatalog {
        pub(crate) fn new() -> Self {
            let col = |name: &str, ty: &str, text_like: bool| ColumnInfo {
                name: name.into(),
                type_sql: ty.into(),
                array: false,
                text_like,
                generated: false,
            };
            BenchCatalog(TableInfo {
                schema: "public".into(),
                name: "bench_items".into(),
                columns: vec![
                    col("id", "bigint", false),
                    col("owner", "uuid", false),
                    col("title", "text", true),
                    col("n", "integer", false),
                    col("created_at", "timestamp with time zone", false),
                ],
            })
        }
    }

    impl Catalog for BenchCatalog {
        fn table(&self, name: &Name) -> Option<&TableInfo> {
            (name.schema.as_deref() == Some("public") && name.name == "bench_items")
                .then_some(&self.0)
        }

        fn function(&self, _: &Name, _: &[&str]) -> Result<&FunctionInfo, FnLookupError> {
            Err(FnLookupError::NotFound)
        }
    }

    fn workload() -> Workload {
        Workload::from_catalog(&BenchCatalog::new(), "public").unwrap()
    }

    fn insert_id(op: &Op) -> i64 {
        let Op::Insert(i) = op else {
            panic!("not an insert: {op:?}")
        };
        i.values[0]["id"].as_i64().expect("an integer id")
    }

    fn eq_filter(op: &Op) -> (&str, &Value) {
        let Op::Select(s) = op else {
            panic!("not a select: {op:?}")
        };
        match s.filter.as_ref() {
            Some(Filter::Cond { col, value, .. }) => (col.as_str(), value),
            other => panic!("not one condition: {other:?}"),
        }
    }

    #[test]
    fn insert_ids_never_repeat() {
        let w = workload();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..10_000 {
            let op = w.next(OpKind::Insert, &mut rng);
            let id = insert_id(&op.op);
            assert!(id >= INSERT_ID_BASE, "{id}");
            assert!(seen.insert(id), "id {id} repeated");
        }
    }

    /// Inserted rows stay out of List100's `n` range, so they don't change its results.
    #[test]
    fn inserts_use_n_outside_the_list_range() {
        let w = workload();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let op = w.next(OpKind::Insert, &mut rng);
        let Op::Insert(insert) = &op.op else {
            panic!("not an insert: {:?}", op.op)
        };
        let n = insert.values[0]["n"].as_i64().unwrap();
        assert_eq!(n, INSERT_N);
        assert!(!(0..N_VALUES).contains(&n));
    }

    #[test]
    fn batch_has_three_ops_in_order() {
        let w = workload();
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let op = w.next(OpKind::Batch, &mut rng);
        assert_eq!(op.kind, OpKind::Batch);
        let Op::Batch(ops) = &op.op else {
            panic!("not a batch: {:?}", op.op)
        };
        assert_eq!(ops.len(), 3);
        assert_eq!(op.plans.len(), 3);
        let (col, id) = eq_filter(&ops[0]);
        assert_eq!(col, "id");
        assert!((1..=SEEDED_ROWS).contains(&id.as_i64().unwrap()));
        let (col, n) = eq_filter(&ops[1]);
        assert_eq!(col, "n");
        assert!((0..N_VALUES).contains(&n.as_i64().unwrap()));
        let Op::Select(list) = &ops[1] else {
            unreachable!()
        };
        assert_eq!(list.limit, Some(LIST_LIMIT));
        assert!(insert_id(&ops[2]) >= INSERT_ID_BASE);
        assert!(op.plans[2].sql.starts_with("WITH r AS (INSERT INTO"));
    }

    /// Every plan returns one `text` value: the planner's select/write wrappers.
    #[test]
    fn single_ops_have_one_plan_and_their_kind() {
        let w = workload();
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        for kind in [OpKind::ByKey, OpKind::List100, OpKind::Insert] {
            let op = w.next(kind, &mut rng);
            assert_eq!(op.kind, kind);
            assert_eq!(op.plans.len(), 1, "{kind:?}");
            assert!(op.plans[0].sql.contains("::text"), "{}", op.plans[0].sql);
        }
        let list = w.next(OpKind::List100, &mut rng);
        assert!(
            list.plans[0].sql.contains("ORDER BY \"id\" ASC"),
            "{}",
            list.plans[0].sql
        );
    }

    /// The same seed draws the same values (the live agreement test relies on it).
    #[test]
    fn same_seed_same_reads() {
        let w = workload();
        let mut a = rand::rngs::StdRng::seed_from_u64(3);
        let mut b = rand::rngs::StdRng::seed_from_u64(3);
        for kind in [OpKind::ByKey, OpKind::List100] {
            assert_eq!(w.next(kind, &mut a).op, w.next(kind, &mut b).op);
        }
    }
}
