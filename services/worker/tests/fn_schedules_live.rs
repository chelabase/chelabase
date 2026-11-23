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
use chelabase_time::{Date, DateExt, Timestamp, TimestampExt};
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
        let secs = Timestamp::now().timestamp() as u32;
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
                compatibility_date: Date::ymd(2026, 10, 1).unwrap(),
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

    /// `n` schedules on the function, each due a minute ago. They run once a year, so a
    /// claimed one isn't due again while a test drains (with a tick every minute, a run that
    /// crossed the minute claimed rows a second time, for the new tick).
    async fn due(&self, branch: BranchId, function: Uuid, n: i32) -> Vec<Uuid> {
        sqlx::query_scalar(
            "INSERT INTO function_schedules (id, branch_id, function_id, cron, timezone, next_run_at)
             SELECT gen_random_uuid(), $1, $2, '0 0 1 1 *', 'UTC', now() - interval '1 minute'
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
        let claimed = client.claim_due_schedules(limit, &[]).await.unwrap();
        if claimed.is_empty() {
            // The other claimer may hold the top of a database's share (skipped, not lost):
            // finished only when nothing is due.
            let due: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM function_schedules WHERE next_run_at <= now()",
            )
            .fetch_one(client.pool())
            .await
            .unwrap();
            if due == 0 {
                return got;
            }
            tokio::task::yield_now().await;
            continue;
        }
        for c in claimed {
            assert!(c.run.is_some(), "an enabled function's due schedule runs");
            assert!(c.next_run_at.unwrap() > Timestamp::now() - chelabase_time::seconds(5));
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
        // The test is about two claimers against each other: both must have claimed rows.
        assert!(
            !a.is_empty() && !b.is_empty(),
            "round {round}: both claimers took part ({} and {})",
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

    let mut claimed = s.client.claim_due_schedules(50, &[]).await.unwrap();
    claimed.sort_by_key(|c| c.function_name.clone());
    let got: Vec<_> = claimed
        .iter()
        .map(|c| (c.id, c.function_name.as_str(), c.run.is_some()))
        .collect();
    assert_eq!(got, vec![(off_row, "off", false), (on_row, "on", true)]);
    for c in &claimed {
        let next: Timestamp =
            sqlx::query_scalar("SELECT next_run_at FROM function_schedules WHERE id = $1")
                .bind(c.id)
                .fetch_one(s.pool())
                .await
                .unwrap();
        assert!(
            next > Timestamp::now(),
            "{}: next_run_at moved on",
            c.function_name
        );
    }
    assert!(s.client.claim_due_schedules(50, &[]).await.unwrap().is_empty());
    s.finish().await;
}

/// One database's due schedules never take more than its share of a claim: with 100 due in one
/// database and 1 in another, the first claim holds the other database's schedule and at most
/// `MAX_IN_FLIGHT_PER_DATABASE` of the big one. A database with runs already in flight gets
/// only what is left of its share, and one at the limit gets none: its schedules aren't locked
/// or advanced (`next_run_at` unchanged) and are claimed, coalesced, once its runs end.
#[tokio::test]
#[ignore]
async fn fn_schedules_claim_shares_slots_across_databases() {
    use chelabase_metadata::MAX_IN_FLIGHT_PER_DATABASE as SHARE;
    let share = SHARE as usize;
    let s = Scratch::new().await;
    let (big, big_branch, big_f) = s.function("big", true).await;
    let (small, small_branch, small_f) = s.function("small", true).await;
    let big_rows = s.due(big_branch, big_f, 100).await;
    let small_row = s.due(small_branch, small_f, 1).await[0];
    let next_of = |ids: Vec<Uuid>| {
        let pool = s.pool().clone();
        async move {
            sqlx::query_as::<_, (Uuid, Timestamp)>(
                "SELECT id, next_run_at FROM function_schedules WHERE id = ANY($1) ORDER BY id",
            )
            .bind(ids)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    let before = next_of(big_rows.clone()).await;

    // The first claim: a limit larger than every due row still gives each database its share.
    let first = s.client.claim_due_schedules(64, &[]).await.unwrap();
    let of = |db: DatabaseId| first.iter().filter(|c| c.database_id == db).count();
    assert_eq!((of(big), of(small)), (share, 1), "the share, and the other database's one");
    assert!(first.iter().any(|c| c.id == small_row));

    // The big database has 5 runs in flight: 3 more of its schedules, never beyond the limit.
    let second = s.client.claim_due_schedules(64, &[(big, share - 3)]).await.unwrap();
    assert_eq!(second.iter().filter(|c| c.database_id == big).count(), 3);

    // At the limit: nothing of it is locked or advanced, whatever the claim limit.
    let claimed: HashSet<Uuid> = first.iter().chain(&second).map(|c| c.id).collect();
    let unclaimed: Vec<Uuid> = big_rows.iter().copied().filter(|id| !claimed.contains(id)).collect();
    let snapshot = next_of(unclaimed.clone()).await;
    let none = s.client.claim_due_schedules(64, &[(big, share)]).await.unwrap();
    assert!(none.is_empty(), "a database at its limit claims nothing");
    assert_eq!(next_of(unclaimed.clone()).await, snapshot, "skipped rows are untouched");
    let over = s.client.claim_due_schedules(64, &[(big, share + 5)]).await.unwrap();
    assert!(over.is_empty(), "over the limit is at the limit");

    // Runs ended: the rest are claimed, share by share, each with its run.
    let later = s.client.claim_due_schedules(64, &[]).await.unwrap();
    assert_eq!(later.len(), share);
    assert!(later.iter().all(|c| c.run.is_some() && unclaimed.contains(&c.id)));
    assert!(before.iter().all(|(_, t)| *t < Timestamp::now()), "they were due");
    s.finish().await;
}

/// The startup report on real Postgres: two schedules stored with crons an older parser
/// accepted (inserted directly), one healthy. After the report the two have their reason in
/// `last_error` and nothing else changed; the first claim after it disables both by today's
/// rule and keeps the reason. (`main` awaits the report before it spawns the claim loop, so the
/// claim here follows it the same way.)
#[tokio::test]
#[ignore]
async fn the_report_precedes_the_first_claim() {
    let s = Scratch::new().await;
    let (_db, branch, function) = s.function("reported", true).await;
    let insert = |cron: &'static str| {
        let pool = s.pool().clone();
        async move {
            sqlx::query_scalar::<_, Uuid>(
                "INSERT INTO function_schedules (id, branch_id, function_id, cron, timezone, next_run_at)
                 VALUES (gen_random_uuid(), $1, $2, $3, 'UTC', now() - interval '1 minute') RETURNING id",
            )
            .bind(branch.as_uuid())
            .bind(function)
            .bind(cron)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let (jan, ell, healthy) = (insert("0 0 * * jan").await, insert("0 0 * * L").await, insert("*/5 * * * *").await);
    let state = |id: Uuid| {
        let pool = s.pool().clone();
        async move {
            sqlx::query_as::<_, (String, String, bool, Option<chelabase_time::Timestamp>, Option<String>)>(
                "SELECT cron, timezone, enabled, next_run_at, last_error FROM function_schedules WHERE id = $1",
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let before = (state(jan).await, state(ell).await, state(healthy).await);

    assert_eq!(worker::schedule_report::run(&s.client).await, Some(2));

    for (id, was) in [(jan, &before.0), (ell, &before.1)] {
        let now = state(id).await;
        assert!(now.4.as_deref().is_some_and(|e| e.starts_with("cron:")), "reason: {:?}", now.4);
        assert_eq!((&now.0, &now.1, now.2, now.3), (&was.0, &was.1, was.2, was.3), "nothing else changed");
    }
    assert_eq!(state(healthy).await, before.2, "the healthy schedule is untouched");

    s.client.claim_due_schedules(64, &[]).await.unwrap();
    for id in [jan, ell] {
        let now = state(id).await;
        assert!(!now.2 && now.3.is_none(), "disabled with no next time");
        assert!(now.4.as_deref().is_some_and(|e| e.starts_with("cron:")), "the reason is kept: {:?}", now.4);
    }
    s.finish().await;
}
