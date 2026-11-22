//! Edge functions m7: scheduled runs. Every [`EVERY`] the loop claims up to [`BATCH`] due
//! schedules ([`chelabase_metadata::MetadataClient::claim_due_schedules`]: `FOR UPDATE SKIP
//! LOCKED`, so any number of worker replicas never claim one tick twice), and for each whose
//! function is enabled on the branch posts `{"type": "scheduled", "cron", "scheduledTime"}` to
//! the functions service's `POST /internal/invoke/{branch_id}/{name}` with the worker's bearer.
//! A run is at most once: the claim has already moved `next_run_at` on, and a failed run is
//! logged, not retried. Missed ticks (the worker was down) coalesce into one run.
//!
//! At most [`MAX_IN_FLIGHT`] runs are in flight per worker: a claim takes no more schedules
//! than there are free slots (none while every slot is busy), so a slow functions service
//! delays the next claims instead of piling up requests. Schedules left unclaimed stay due and
//! run, coalesced, once a slot frees.
//!
//! One database never holds more than [`chelabase_metadata::MAX_IN_FLIGHT_PER_DATABASE`] of
//! those slots: [`DatabaseRuns`] counts each database's runs in flight (a [`RunGuard`] held by
//! the run's task, released on every way out of it: done, failed, timed out, panicked or
//! dropped at shutdown), and a claim gives each database only what is left of its share, so
//! one tenant's due schedules can't fill every slot. The ones left out stay due, untouched.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chelabase_metadata::MetadataClient;
use chelabase_types::{BranchId, DatabaseId};
use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;
use tracing::{debug, warn};

/// How often the loop claims.
pub const EVERY: Duration = Duration::from_secs(5);
/// At most this many schedules per claim; a full batch claims again at once.
pub const BATCH: i64 = 50;
/// At most this many runs in flight at once, per worker.
pub const MAX_IN_FLIGHT: usize = 64;
/// The longest a run's request may take (the functions service's own deadline is 30 s).
const INVOKE_TIMEOUT: Duration = Duration::from_secs(60);

/// The invocation's body: what the runtime hands the `scheduled` handler as its controller.
pub fn event_body(cron: &str, scheduled: DateTime<Utc>) -> serde_json::Value {
    serde_json::json!({
        "type": "scheduled",
        "cron": cron,
        "scheduledTime": scheduled.timestamp_millis(),
    })
}

/// Where a run goes.
#[async_trait::async_trait]
pub trait Invoker: Send + Sync + 'static {
    /// The functions service's status, or why it couldn't be reached.
    async fn invoke(
        &self,
        branch: BranchId,
        name: &str,
        body: serde_json::Value,
    ) -> Result<u16, String>;
}

/// The functions service's control listener (`CHELABASE_FUNCTIONS_INTERNAL_URL`) with the
/// worker's bearer (`CHELABASE_WORKER_FUNCTIONS_TOKEN[_FILE]`).
pub struct HttpInvoker {
    url: String,
    token: String,
    http: chelabase_http::Client,
}

impl HttpInvoker {
    pub fn new(url: &str, token: String) -> anyhow::Result<Self> {
        Ok(Self {
            url: url.trim_end_matches('/').to_owned(),
            token,
            http: chelabase_http::client_builder()
                .connect_timeout(Duration::from_secs(5))
                .build()?,
        })
    }
}

#[async_trait::async_trait]
impl Invoker for HttpInvoker {
    async fn invoke(
        &self,
        branch: BranchId,
        name: &str,
        body: serde_json::Value,
    ) -> Result<u16, String> {
        let resp = self
            .http
            .post(format!("{}/internal/invoke/{branch}/{name}", self.url))
            .bearer_auth(&self.token)
            .timeout(INVOKE_TIMEOUT)
            .json(&body)
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        Ok(resp.status().as_u16())
    }
}

/// The runs in flight per database. Cheap to clone (shared).
#[derive(Clone, Default)]
pub struct DatabaseRuns(Arc<Mutex<HashMap<DatabaseId, usize>>>);

/// One run's count in [`DatabaseRuns`]; dropping it (on any exit of the run's task) gives it
/// back.
pub struct RunGuard {
    runs: DatabaseRuns,
    database: DatabaseId,
}

impl DatabaseRuns {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<DatabaseId, usize>> {
        // The map is only ever incremented and decremented: a poisoned lock holds valid data.
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts a run of `database` until the guard drops.
    pub fn enter(&self, database: DatabaseId) -> RunGuard {
        *self.map().entry(database).or_insert(0) += 1;
        RunGuard {
            runs: self.clone(),
            database,
        }
    }

    /// Every database with runs in flight and how many, for the claim.
    pub fn in_flight(&self) -> Vec<(DatabaseId, usize)> {
        self.map().iter().map(|(d, n)| (*d, *n)).collect()
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let mut map = self.runs.map();
        if let Some(n) = map.get_mut(&self.database) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.database);
            }
        }
    }
}

/// How many schedules the next claim may take: the free run slots, at most [`BATCH`].
pub fn claim_limit(permits: &Semaphore) -> i64 {
    (permits.available_permits() as i64).min(BATCH)
}

/// Starts the runs of `claimed` (a schedule that only advanced has none), each in a slot of
/// `permits` held until its request ends; waits for a slot when every one is busy, never for
/// a run.
pub async fn start_runs(
    permits: &Arc<Semaphore>,
    runs: &DatabaseRuns,
    invoker: &Arc<dyn Invoker>,
    claimed: &[chelabase_metadata::ClaimedSchedule],
) {
    for s in claimed {
        let Some(tick) = s.run else {
            debug!(schedule = %s.id, branch_id = %s.branch_id, function = %s.function_name, "schedule advanced without a run");
            continue;
        };
        // Counted before the wait for a slot, so the next claim sees this run.
        let counted = runs.enter(s.database_id);
        let Ok(slot) = permits.clone().acquire_owned().await else {
            return;
        };
        let invoker = invoker.clone();
        let (id, branch, name) = (s.id, s.branch_id, s.function_name.clone());
        let body = event_body(&s.cron, tick);
        tokio::spawn(async move {
            let (_slot, _counted) = (slot, counted);
            match invoker.invoke(branch, &name, body).await {
                Ok(status) if (200..300).contains(&status) => {
                    debug!(schedule = %id, branch_id = %branch, function = %name, "scheduled run done");
                }
                Ok(status) => {
                    warn!(schedule = %id, branch_id = %branch, function = %name, status, "scheduled run failed");
                }
                Err(e) => {
                    warn!(schedule = %id, branch_id = %branch, function = %name, "scheduled run didn't reach the functions service: {e}");
                }
            }
        });
    }
}

/// One claim of up to [`claim_limit`] schedules: advances every due schedule it took and
/// starts the runs (without waiting for them). Whether it took as many as it could, so the
/// loop claims again at once (`false` when no slot was free).
pub async fn claim_once(
    metadata: &MetadataClient,
    invoker: &Arc<dyn Invoker>,
    permits: &Arc<Semaphore>,
    runs: &DatabaseRuns,
) -> Result<bool, chelabase_metadata::MetadataError> {
    let limit = claim_limit(permits);
    if limit == 0 {
        debug!("every scheduled-run slot is busy: claiming later");
        return Ok(false);
    }
    let claimed = metadata.claim_due_schedules(limit, &runs.in_flight()).await?;
    start_runs(permits, runs, invoker, &claimed).await;
    Ok(claimed.len() as i64 >= limit)
}

/// The loop: a claim every [`EVERY`], and again at once after a full one.
pub async fn schedule_loop(metadata: MetadataClient, invoker: Arc<dyn Invoker>) {
    let permits = Arc::new(Semaphore::new(MAX_IN_FLIGHT));
    let runs = DatabaseRuns::default();
    let mut every = tokio::time::interval(EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        loop {
            match claim_once(&metadata, &invoker, &permits, &runs).await {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => {
                    warn!("claiming due schedules failed: {e}");
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_metadata::{Schedule, advance};
    use chrono::TimeZone;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn runs(cron: &str, tz: &str, from: &str, n: usize) -> Vec<DateTime<Utc>> {
        let s = Schedule::parse(cron, tz).unwrap();
        let mut t = at(from);
        (0..n)
            .map(|_| {
                t = s.next_after(t).unwrap();
                t
            })
            .collect()
    }

    /// Spring forward (29 March 2026, 01:00 GMT becomes 02:00 BST): a daily 01:30, which
    /// doesn't exist that night, still runs once that day; a 09:00 stays 09:00 local.
    #[test]
    fn fn_schedules_next_run_across_spring_forward_in_london() {
        assert_eq!(
            runs("30 1 * * *", "Europe/London", "2026-03-28T00:00:00Z", 3),
            vec![
                at("2026-03-28T01:30:00Z"),
                at("2026-03-29T01:00:00Z"),
                at("2026-03-30T00:30:00Z"),
            ]
        );
        assert_eq!(
            runs("0 9 * * *", "Europe/London", "2026-03-28T00:00:00Z", 2),
            vec![at("2026-03-28T09:00:00Z"), at("2026-03-29T08:00:00Z")]
        );
    }

    /// Fall back (25 October 2026, 02:00 BST becomes 01:00 GMT): a daily 01:30, which that
    /// night happens twice, runs once.
    #[test]
    fn fn_schedules_next_run_across_fall_back_in_london() {
        assert_eq!(
            runs("30 1 * * *", "Europe/London", "2026-10-24T12:00:00Z", 3),
            vec![
                at("2026-10-25T00:30:00Z"),
                at("2026-10-26T01:30:00Z"),
                at("2026-10-27T01:30:00Z"),
            ]
        );
        assert_eq!(
            runs("0 9 * * *", "Europe/London", "2026-10-24T12:00:00Z", 2),
            vec![at("2026-10-25T09:00:00Z"), at("2026-10-26T09:00:00Z")]
        );
    }

    /// Five missed minutes are one run, of the first missed tick, and the next time is the
    /// first tick after now: a down worker never replays.
    #[test]
    fn fn_schedules_missed_ticks_coalesce_into_one_run() {
        let due = at("2026-11-07T10:00:00Z");
        let now = at("2026-11-07T10:05:30Z");
        let step = advance("* * * * *", "UTC", Some(due), now, true);
        assert_eq!(step.run, Some(due));
        assert_eq!(step.next, Some(at("2026-11-07T10:06:00Z")));
        assert!(step.enabled);
        // A disabled function: no run, and the time still moves on.
        let step = advance("* * * * *", "UTC", Some(due), now, false);
        assert_eq!(
            (step.run, step.next),
            (None, Some(at("2026-11-07T10:06:00Z")))
        );
        // No time yet: it gets one, nothing runs.
        let step = advance("*/15 * * * *", "UTC", None, now, true);
        assert_eq!(
            (step.run, step.next),
            (None, Some(at("2026-11-07T10:15:00Z")))
        );
        // A cron that no longer parses: switched off, nothing runs.
        let step = advance("nope", "UTC", Some(due), now, true);
        assert_eq!((step.run, step.next, step.enabled), (None, None, false));
    }

    /// A bad cron or time zone names its field; 6-field (seconds) and 7-field (years) crons
    /// aren't 5-field.
    #[test]
    fn fn_schedules_invalid_cron_or_timezone_names_the_field() {
        for cron in [
            "",
            "* * * *",
            "0 * * * * *",
            "0 0 * * * * 2030",
            "61 * * * *",
            "x",
        ] {
            let err = Schedule::parse(cron, "UTC").unwrap_err();
            assert_eq!(err.field(), "cron", "{cron:?}");
            assert!(err.to_string().starts_with("cron: "), "{err}");
        }
        for tz in ["", "Mars/Olympus", "europe/london", "UTC+1"] {
            let err = Schedule::parse("* * * * *", tz).unwrap_err();
            assert_eq!(err.field(), "timezone", "{tz:?}");
        }
        assert!(Schedule::parse(&"1".repeat(101), "UTC").is_err());
        Schedule::parse("*/5 9-17 * * MON-FRI", "Europe/London").unwrap();
    }

    /// Holds every run until released, counting how many are in flight at once.
    struct Held {
        now: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
        done: std::sync::atomic::AtomicUsize,
        release: tokio::sync::Semaphore,
    }

    #[async_trait::async_trait]
    impl Invoker for Held {
        async fn invoke(&self, _: BranchId, _: &str, _: serde_json::Value) -> Result<u16, String> {
            use std::sync::atomic::Ordering::SeqCst;
            let n = self.now.fetch_add(1, SeqCst) + 1;
            self.most.fetch_max(n, SeqCst);
            self.release.acquire().await.unwrap().forget();
            self.now.fetch_sub(1, SeqCst);
            self.done.fetch_add(1, SeqCst);
            Ok(200)
        }
    }

    fn claimed(run: bool) -> chelabase_metadata::ClaimedSchedule {
        claimed_in(DatabaseId::new(), run)
    }

    fn claimed_in(database: DatabaseId, run: bool) -> chelabase_metadata::ClaimedSchedule {
        chelabase_metadata::ClaimedSchedule {
            id: uuid::Uuid::new_v4(),
            branch_id: BranchId::new(),
            database_id: database,
            function_name: "f".into(),
            cron: "* * * * *".into(),
            run: run.then(Utc::now),
            next_run_at: None,
        }
    }

    /// A slow functions service never piles up runs: at most the cap are in flight, a claim
    /// takes no more than the free slots (none when full), and every claimed run still runs.
    #[tokio::test]
    async fn fn_schedules_runs_in_flight_are_capped() {
        use std::sync::atomic::Ordering::SeqCst;
        let held = Arc::new(Held {
            now: 0.into(),
            most: 0.into(),
            done: 0.into(),
            release: tokio::sync::Semaphore::new(0),
        });
        let invoker: Arc<dyn Invoker> = held.clone();
        let permits = Arc::new(tokio::sync::Semaphore::new(3));
        assert_eq!(claim_limit(&permits), 3);

        // Five claimed runs (and one that only advanced) against three slots.
        let batch: Vec<_> = (0..5)
            .map(|_| claimed(true))
            .chain([claimed(false)])
            .collect();
        let starting = {
            let (permits, invoker) = (permits.clone(), invoker.clone());
            tokio::spawn(async move { start_runs(&permits, &DatabaseRuns::default(), &invoker, &batch).await })
        };
        while held.now.load(SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(claim_limit(&permits), 0, "a full cap claims nothing");
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            held.now.load(SeqCst),
            3,
            "no fourth run while three are in flight"
        );

        held.release.add_permits(5);
        starting.await.unwrap();
        while held.done.load(SeqCst) < 5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(held.most.load(SeqCst), 3);
        // Every slot is back once the runs end.
        while permits.available_permits() < 3 {
            tokio::task::yield_now().await;
        }
        assert_eq!(claim_limit(&permits), 3);
        assert_eq!(
            claim_limit(&Arc::new(tokio::sync::Semaphore::new(500))),
            BATCH
        );
    }

    #[test]
    fn fn_schedules_event_body_is_the_runtime_contract() {
        let t = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(
            event_body("* * * * *", t),
            serde_json::json!({"type": "scheduled", "cron": "* * * * *", "scheduledTime": 1767225600000_i64})
        );
    }

    /// Answers each call by its name: `fail` a 500, `down` an error, `boom` a panic, `block`
    /// waits until cancelled, anything else 200.
    struct Scripted;

    #[async_trait::async_trait]
    impl Invoker for Scripted {
        async fn invoke(&self, _: BranchId, name: &str, _: serde_json::Value) -> Result<u16, String> {
            match name {
                "fail" => Ok(500),
                "down" => Err("unreachable".into()),
                "boom" => panic!("the run panicked"),
                "block" => std::future::pending().await,
                _ => Ok(200),
            }
        }
    }

    fn named(database: DatabaseId, name: &str) -> chelabase_metadata::ClaimedSchedule {
        chelabase_metadata::ClaimedSchedule {
            function_name: name.into(),
            ..claimed_in(database, true)
        }
    }

    /// Whatever way a run ends (a failure status, an unreachable service, a panic, the task
    /// cancelled at shutdown), its database's count drops, so no database stays reported busy.
    #[tokio::test]
    async fn a_failed_or_panicking_run_releases_its_database_slot() {
        let runs = DatabaseRuns::default();
        let permits = Arc::new(Semaphore::new(8));
        let invoker: Arc<dyn Invoker> = Arc::new(Scripted);
        let db = DatabaseId::new();
        let batch: Vec<_> = ["ok", "fail", "down", "boom"].iter().map(|n| named(db, n)).collect();
        start_runs(&permits, &runs, &invoker, &batch).await;
        while !runs.in_flight().is_empty() || permits.available_permits() < 8 {
            tokio::task::yield_now().await;
        }

        // A run still going is counted; a guard gives its count back whenever it drops.
        let batch = [named(db, "block")];
        start_runs(&permits, &runs, &invoker, &batch).await;
        assert_eq!(runs.in_flight(), vec![(db, 1)]);
        let guard = runs.enter(db);
        assert_eq!(runs.in_flight(), vec![(db, 2)]);
        drop(guard);
        assert_eq!(runs.in_flight(), vec![(db, 1)]);

        // A start_runs future dropped while it waits for a slot gives its count back too.
        let tight = Arc::new(Semaphore::new(0));
        let waiting = {
            let (runs, invoker) = (runs.clone(), invoker.clone());
            let batch = [named(DatabaseId::new(), "ok")];
            tokio::spawn(async move { start_runs(&tight, &runs, &invoker, &batch).await })
        };
        while runs.in_flight().len() < 2 {
            tokio::task::yield_now().await;
        }
        waiting.abort();
        let _ = waiting.await;
        assert_eq!(runs.in_flight(), vec![(db, 1)]);
    }

    /// The claim is told which databases have runs in flight and how many, and a database's
    /// count rises with each run started and falls as each ends.
    #[tokio::test]
    async fn busy_databases_are_reported_to_the_claim() {
        use std::sync::atomic::Ordering::SeqCst;
        let held = Arc::new(Held {
            now: 0.into(),
            most: 0.into(),
            done: 0.into(),
            release: tokio::sync::Semaphore::new(0),
        });
        let invoker: Arc<dyn Invoker> = held.clone();
        let permits = Arc::new(Semaphore::new(64));
        let runs = DatabaseRuns::default();
        let (a, b) = (DatabaseId::new(), DatabaseId::new());
        let batch: Vec<_> = (0..3).map(|_| claimed_in(a, true)).chain([claimed_in(b, true)]).collect();
        start_runs(&permits, &runs, &invoker, &batch).await;
        while held.now.load(SeqCst) < 4 {
            tokio::task::yield_now().await;
        }
        let seen: HashMap<_, _> = runs.in_flight().into_iter().collect();
        assert_eq!(seen, HashMap::from([(a, 3), (b, 1)]));
        held.release.add_permits(4);
        while held.done.load(SeqCst) < 4 || !runs.in_flight().is_empty() {
            tokio::task::yield_now().await;
        }
    }
}
