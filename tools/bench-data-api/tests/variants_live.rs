//! Live checks of the four variants against a seeded bench database (`setup::seed` already
//! run). Ignored by default; run through `just workbench run` with `BENCH_OWNER_URL`, `BENCH_BRANCH`,
//! `CHELABASE_INTERNAL_URL` and `CHELABASE_REST_INTERNAL_TOKEN` set.

use std::sync::Arc;

use bench_data_api::variants::{
    ALL_VARIANTS, Exec, RTarget, STALE_VERSION, VariantA, VariantB, VariantC, VariantD,
    VariantKind, Variants, fixed_claims, sqlstate,
};
use bench_data_api::workload::{ALL_OPS, BenchOp, INSERT_ID_BASE, OpKind, Workload};
use chelabase_query::Op;
use chelabase_query::sql::Plan;
use rand::SeedableRng;
use rest::conn::{ConnInfo, ConnSource, InternalSource};
use serde_json::Value;
use sqlx::Connection;

const MAX_ROWS: i64 = 1000;

struct Env {
    info: ConnInfo,
    target: RTarget,
    owner: sqlx::PgConnection,
    version: uuid::Uuid,
    workload: Workload,
    timeout_ms: i64,
}

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"))
}

async fn env() -> Env {
    let branch: uuid::Uuid = var("BENCH_BRANCH").parse().expect("BENCH_BRANCH is a uuid");
    let source = InternalSource::new(
        &var("CHELABASE_INTERNAL_URL"),
        &var("CHELABASE_REST_INTERNAL_TOKEN"),
    );
    let info = source.fetch(branch).await.expect("connection info");
    let mut owner = sqlx::PgConnection::connect(&var("BENCH_OWNER_URL"))
        .await
        .expect("the owner connection");
    let version = sqlx::query_scalar("SELECT version FROM chelabase.schema_version")
        .fetch_one(&mut owner)
        .await
        .expect("the schema version");
    let default_schema = info
        .served(&info.database)
        .map_or("public", |d| d.default_schema.as_str())
        .to_owned();
    let workload = Workload::build(&var("BENCH_OWNER_URL"), &default_schema)
        .await
        .expect("the workload");
    let timeout_ms = info.settings.statement_timeout_ms;
    let target = RTarget {
        internal_url: var("CHELABASE_INTERNAL_URL"),
        internal_token: var("CHELABASE_REST_INTERNAL_TOKEN"),
        branch,
    };
    Env {
        info,
        target,
        owner,
        version,
        workload,
        timeout_ms,
    }
}

impl Env {
    async fn row_exists(&mut self, id: i64) -> bool {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM bench_items WHERE id = $1)")
            .bind(id)
            .fetch_one(&mut self.owner)
            .await
            .expect("the row check")
    }

    fn c(&self, version: uuid::Uuid) -> VariantC {
        VariantC::connect(
            &self.info,
            1,
            &fixed_claims(),
            version,
            MAX_ROWS,
            self.timeout_ms,
        )
        .expect("C")
    }

    fn d(&self, version: uuid::Uuid) -> VariantD {
        VariantD::connect(
            &self.info,
            1,
            &fixed_claims(),
            version,
            MAX_ROWS,
            self.timeout_ms,
        )
        .expect("D")
    }

    /// An op that runs `sql` (no parameters, one `text` value) in place of its plans.
    fn raw(&self, sql: &str) -> BenchOp {
        let mut rng = rand::rngs::StdRng::seed_from_u64(0);
        let base = self.workload.next(OpKind::ByKey, &mut rng);
        BenchOp {
            kind: base.kind,
            op: base.op,
            body: base.body,
            plans: Arc::new(vec![Plan {
                sql: sql.into(),
                binds: vec![],
                capped_sql: String::new(),
            }]),
        }
    }
}

fn insert_id(op: &Op) -> i64 {
    let insert = match op {
        Op::Insert(i) => i,
        Op::Batch(ops) => match &ops[2] {
            Op::Insert(i) => i,
            other => panic!("not an insert: {other:?}"),
        },
        other => panic!("no insert: {other:?}"),
    };
    insert.values[0]["id"].as_i64().expect("an id")
}

/// Inserted ids (from the counter) differ between runs: blank them.
fn mask_inserted_ids(v: &mut Value) {
    match v {
        Value::Object(m) => {
            for (k, v) in m.iter_mut() {
                if k == "id" && v.as_i64().is_some_and(|id| id >= INSERT_ID_BASE) {
                    *v = Value::Null;
                } else {
                    mask_inserted_ids(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_inserted_ids),
        _ => {}
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn variants_agree() {
    let e = env().await;
    // The four Phase 1 variants and the shipped path (R): same JSON for the same ops.
    let mut kinds = ALL_VARIANTS.to_vec();
    kinds.push(VariantKind::R);
    let variants = Variants::connect(
        &kinds,
        &e.info,
        &e.target,
        2,
        &fixed_claims(),
        e.version,
        MAX_ROWS,
        e.timeout_ms,
    )
    .await
    .expect("the variants");
    assert!(variants.ping().await.is_ok());
    for (k, kind) in ALL_OPS.into_iter().enumerate() {
        let mut outputs: Vec<(String, Vec<String>)> = Vec::new();
        for v in &variants.all {
            let mut rng = rand::rngs::StdRng::seed_from_u64(100 + k as u64);
            let mut out = Vec::new();
            for _ in 0..20 {
                let op = e.workload.next(kind, &mut rng);
                out.push(
                    v.exec(&op)
                        .await
                        .unwrap_or_else(|err| panic!("{:?} {kind:?}: {err:#}", v.kind())),
                );
            }
            outputs.push((format!("{:?}", v.kind()), out));
        }
        let (first_name, first) = &outputs[0];
        for (name, out) in &outputs[1..] {
            for (i, (a, b)) in first.iter().zip(out).enumerate() {
                if matches!(kind, OpKind::ByKey | OpKind::List100) {
                    assert_eq!(a, b, "{kind:?} #{i}: {first_name} vs {name}");
                } else {
                    let mut a: Value = serde_json::from_str(a).expect("JSON");
                    let mut b: Value = serde_json::from_str(b).expect("JSON");
                    mask_inserted_ids(&mut a);
                    mask_inserted_ids(&mut b);
                    assert_eq!(a, b, "{kind:?} #{i}: {first_name} vs {name}");
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn b_claims_round_trip() {
    let e = env().await;
    let claims = serde_json::json!({"sub": "it's", "path": "a\\b", "name": "Jürgen"});
    let b = VariantB::connect(&e.info, 1, &claims, MAX_ROWS, e.timeout_ms).expect("B");
    let out = b
        .exec(&e.raw("SELECT current_setting('request.jwt.claims')"))
        .await
        .expect("B runs");
    assert_eq!(out, claims.to_string());
}

#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn c_pipeline_prepares_before_sending() {
    let mut e = env().await;
    let c = e.c(e.version);
    let mut rng = rand::rngs::StdRng::seed_from_u64(1);
    let op = e.workload.next(OpKind::Insert, &mut rng);
    let id = insert_id(&op.op);
    c.exec(&op).await.expect("the first insert on a fresh pool");
    assert!(e.row_exists(id).await, "COMMIT overtook the insert");
}

#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn c_stale_version_rolls_back() {
    let mut e = env().await;
    let c = e.c(uuid::Uuid::new_v4());
    let mut rng = rand::rngs::StdRng::seed_from_u64(2);
    let op = e.workload.next(OpKind::Insert, &mut rng);
    let id = insert_id(&op.op);
    let err = c.exec(&op).await.expect_err("a stale version fails");
    assert_eq!(sqlstate(&err).as_deref(), Some(STALE_VERSION), "{err:#}");
    assert!(!e.row_exists(id).await, "the insert was rolled back");
    // The same connection (pool of 1) ended its transaction: CB001 again, not 25P02.
    let err = c.exec(&e.raw("SELECT 'x'")).await.expect_err("still stale");
    assert_eq!(sqlstate(&err).as_deref(), Some(STALE_VERSION), "{err:#}");
}

#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn d_stale_version_rolls_back() {
    let mut e = env().await;
    let d = e.d(uuid::Uuid::new_v4());
    let mut rng = rand::rngs::StdRng::seed_from_u64(3);
    let op = e.workload.next(OpKind::Insert, &mut rng);
    let id = insert_id(&op.op);
    let err = d.exec(&op).await.expect_err("a stale version fails");
    assert_eq!(sqlstate(&err).as_deref(), Some(STALE_VERSION), "{err:#}");
    assert!(!e.row_exists(id).await, "the insert was rolled back");
    // The same connection (pool of 1) ended its transaction.
    let err = d.exec(&e.raw("SELECT 'x'")).await.expect_err("still stale");
    assert_eq!(sqlstate(&err).as_deref(), Some(STALE_VERSION), "{err:#}");
}

fn empty(out: &str) -> bool {
    out == "null" || out.is_empty()
}

#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn session_state_doesnt_leak() {
    let e = env().await;
    let set = e.raw("SELECT set_config('x.y', '1', false)");
    let read = e.raw("SELECT current_setting('x.y', true)");

    // Pools of one: the second request reuses the first's connection.
    let c = e.c(e.version);
    assert_eq!(c.exec(&set).await.expect("C sets"), "1");
    let out = c.exec(&read).await.expect("C reads");
    assert!(empty(&out), "C leaked x.y: {out:?}");

    let b = VariantB::connect(&e.info, 1, &fixed_claims(), MAX_ROWS, e.timeout_ms).expect("B");
    assert_eq!(b.exec(&set).await.expect("B sets"), "1");
    let out = b.exec(&read).await.expect("B reads");
    assert!(empty(&out), "B leaked x.y: {out:?}");

    // A resets after release.
    let a = VariantA::connect(&e.info, 1, &fixed_claims(), MAX_ROWS, e.timeout_ms);
    assert_eq!(a.exec(&set).await.expect("A sets"), "1");
    let out = a.exec(&read).await.expect("A reads");
    assert!(empty(&out), "A leaked x.y: {out:?}");
}
