//! Edge functions m7: the schedule claim on real Postgres. Two claimers in parallel never take
//! the same row (1,000 due schedules, random batch sizes, several rounds), every due row is
//! claimed exactly once, and a claim skips deleted branches and databases and advances a
//! disabled function's schedule without a run. Each test makes its own scratch database
//! (`cm_…`, as `members_live` names and sweeps them) on the workbench stack's metadata
//! Postgres.
//!
//! ```text
//! just workbench test -p worker --test fn_schedules_live -- --run-ignored only
//! ```

use std::collections::HashSet;

use chelabase_metadata::{
    DEMO_PROJECT_ID, DEMO_TENANT_ID, FunctionKind, MetadataClient, MetadataConfig, NewDeployment,
};
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chrono::{NaiveDate, Utc};
use rand::RngExt;
use sqlx::postgres::PgPoolOptions;
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

fn url_for(name: &str) -> String {
    let base = url();
    let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
    let query = tail
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    format!("{head}/{name}{query}")
}

struct Scratch {
    meta: PgPool,
    name: String,
    client: MetadataClient,
}

impl Scratch {
    async fn new() -> Scratch {
        let meta = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url())
            .await
            .expect("metadata Postgres reachable (is the workbench stack up?)");
        let secs = Utc::now().timestamp() as u32;
        let name = format!("cm_{secs:08x}{}", &Uuid::new_v4().simple().to_string()[..4]);
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&meta)
            .await
            .unwrap();
        let client = Self::connect(&name).await;
        client.migrate().await.unwrap();
        Scratch { meta, name, client }
    }

    async fn connect(name: &str) -> MetadataClient {
        MetadataClient::connect(&MetadataConfig {
            database_url: url_for(name),
        })
        .await
        .unwrap()
    }

    fn pool(&self) -> &PgPool {
        self.client.pool()
    }

    async fn finish(self) {
        self.client.pool().close().await;
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&self.meta)
        .await
        .unwrap();
    }

    /// A ready database with function `name` deployed and enabled (or not) on main.
    async fn function(&self, name: &str, enabled: bool) -> (DatabaseId, BranchId, Uuid) {
        let id = DatabaseId::new();
        self.client
            .insert_database_pending(
                id,
                TenantId(DEMO_TENANT_ID),
                ProjectId(DEMO_PROJECT_ID),
                &format!("d-{id}"),
            )
            .await
            .unwrap();
        sqlx::query("UPDATE databases SET status = 'ready' WHERE id = $1")
            .bind(id.as_uuid())
            .execute(self.pool())
            .await
            .unwrap();
        let main = BranchId::from(id);
        sqlx::query("UPDATE branches SET status = 'ready' WHERE id = $1")
            .bind(main.as_uuid())
            .execute(self.pool())
            .await
            .unwrap();
        let f = self
            .client
            .create_function(id, name, FunctionKind::Function)
            .await
            .unwrap()
            .id;
        let spec = serde_json::json!({});
        let sha = "a".repeat(64);
        let d = self
            .client
            .create_deployment(&NewDeployment {
                id: Uuid::new_v4(),
                function_id: f,
                bundle_sha256: Some(&sha),
                bundle_size: Some(10),
                spec: &spec,
                compatibility_date: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
                created_by: "member-1",
                note: None,
            })
            .await
            .unwrap()
            .id;
        self.client.set_active_deployment(main, f, d).await.unwrap();
        assert!(self.client.set_enabled(main, f, enabled).await.unwrap());
        (id, main, f)
    }

    /// `n` schedules on the function, each due a minute ago.
    async fn due(&self, branch: BranchId, function: Uuid, n: i32) -> Vec<Uuid> {
        sqlx::query_scalar(
            "INSERT INTO function_schedules (id, branch_id, function_id, cron, timezone, next_run_at)
             SELECT gen_random_uuid(), $1, $2, '* * * * *', 'UTC', now() - interval '1 minute'
             FROM generate_series(1, $3) RETURNING id",
        )
        .bind(branch.as_uuid())
        .bind(function)
        .bind(n)
        .fetch_all(self.pool())
        .await
        .unwrap()
    }
}

/// Claims until a claim comes back empty, with a random batch size each time.
async fn drain(client: MetadataClient) -> Vec<Uuid> {
    let mut got = Vec::new();
    loop {
        let limit = rand::rng().random_range(1..=60);
        let claimed = client.claim_due_schedules(limit).await.unwrap();
        if claimed.is_empty() {
            return got;
        }
        for c in claimed {
            assert!(c.run.is_some(), "an enabled function's due schedule runs");
            assert!(c.next_run_at.unwrap() > Utc::now() - chrono::Duration::seconds(5));
            got.push(c.id);
        }
    }
}

/// The invariant behind "several replicas never double-run a tick": two claimers draining
/// the same 1,000 due rows at once never return one row twice, and between them they claim
/// every row.
#[tokio::test]
#[ignore]
async fn fn_schedules_two_claimers_never_return_the_same_row() {
    let s = Scratch::new().await;
    let (_, main, f) = s.function("tick", true).await;
    for round in 0..3 {
        let all: HashSet<Uuid> = s.due(main, f, 1000).await.into_iter().collect();
        let (a, b) = (
            Scratch::connect(&s.name).await,
            Scratch::connect(&s.name).await,
        );
        let (a, b) = tokio::join!(tokio::spawn(drain(a)), tokio::spawn(drain(b)));
        let (a, b) = (a.unwrap(), b.unwrap());
        let (sa, sb): (HashSet<_>, HashSet<_>) =
            (a.iter().copied().collect(), b.iter().copied().collect());
        assert_eq!(
            sa.len(),
            a.len(),
            "round {round}: a claimer saw a row twice"
        );
        assert_eq!(
            sb.len(),
            b.len(),
            "round {round}: a claimer saw a row twice"
        );
        assert!(
            sa.is_disjoint(&sb),
            "round {round}: both claimers took {:?}",
            sa.intersection(&sb).next()
        );
        let union: HashSet<_> = sa.union(&sb).copied().collect();
        assert_eq!(union, all, "round {round}: every due row is claimed");
        assert!(
            !a.is_empty() && !b.is_empty(),
            "round {round}: both claimers took part ({} / {})",
            a.len(),
            b.len()
        );
        sqlx::query("DELETE FROM function_schedules")
            .execute(s.pool())
            .await
            .unwrap();
    }
    s.finish().await;
}

/// A disabled function's schedule advances with no run; a deleted branch's or database's
/// schedules are never claimed; a row not yet due isn't either.
#[tokio::test]
#[ignore]
async fn fn_schedules_claim_skips_dead_rows_and_runs_only_enabled_functions() {
    let s = Scratch::new().await;
    let (_, on_branch, on) = s.function("on", true).await;
    let (_, off_branch, off) = s.function("off", false).await;
    let (dead_db, dead_db_branch, dead_db_f) = s.function("gone", true).await;
    let (_, dead_branch, dead_f) = s.function("gone-branch", true).await;
    let on_row = s.due(on_branch, on, 1).await[0];
    let off_row = s.due(off_branch, off, 1).await[0];
    s.due(dead_db_branch, dead_db_f, 1).await;
    s.due(dead_branch, dead_f, 1).await;
    sqlx::query("UPDATE databases SET status = 'deleting' WHERE id = $1")
        .bind(dead_db.as_uuid())
        .execute(s.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE branches SET status = 'deleted' WHERE id = $1")
        .bind(dead_branch.as_uuid())
        .execute(s.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO function_schedules (id, branch_id, function_id, cron, timezone, next_run_at)
         VALUES (gen_random_uuid(), $1, $2, '* * * * *', 'UTC', now() + interval '1 hour')",
    )
    .bind(on_branch.as_uuid())
    .bind(on)
    .execute(s.pool())
    .await
    .unwrap();

    let mut claimed = s.client.claim_due_schedules(50).await.unwrap();
    claimed.sort_by_key(|c| c.function_name.clone());
    let got: Vec<_> = claimed
        .iter()
        .map(|c| (c.id, c.function_name.as_str(), c.run.is_some()))
        .collect();
    assert_eq!(got, vec![(off_row, "off", false), (on_row, "on", true)]);
    for c in &claimed {
        let next: chrono::DateTime<Utc> =
            sqlx::query_scalar("SELECT next_run_at FROM function_schedules WHERE id = $1")
                .bind(c.id)
                .fetch_one(s.pool())
                .await
                .unwrap();
        assert!(
            next > Utc::now(),
            "{}: next_run_at moved on",
            c.function_name
        );
    }
    assert!(s.client.claim_due_schedules(50).await.unwrap().is_empty());
    s.finish().await;
}
