//! The startup report of stored schedules whose cron or time zone the current parser rejects.
//!
//! A schedule stored under an older parser can stop parsing after an upgrade. Such a schedule
//! is switched off the first time it comes due (`advance`); this report tells an operator
//! beforehand. Before the schedule loop makes its first claim, [`run`] reads every schedule
//! (enabled or not, a page at a time), logs one `warn` line for each that fails
//! [`Schedule::parse`] and one summary line, and has the metadata write each one's reason to
//! `last_error` ([`MetadataClient::record_schedule_errors`], which derives the reason itself
//! from the locked row). It changes nothing else: the schedules then follow the invalid-cron
//! rule unchanged. A failed read or write is logged and never stops the worker.
//!
//! Only **enabled** failing schedules are passed to `record_schedule_errors`; a failing
//! schedule that is already disabled is logged and never gets a `last_error`. That keeps the
//! invariant "`last_error` set implies the worker switched the schedule off" true, so
//! Studio's "Stopped" block (whose edit sends `enabled: true`) never turns on a schedule a
//! user switched off themselves.

use chelabase_metadata::{
    MetadataClient, MetadataError, Schedule, ScheduleReportRow,
};
use tracing::{error, info, warn};
use uuid::Uuid;

/// Schedules read per page.
pub const PAGE: i64 = 500;

/// The two metadata calls the report makes.
#[async_trait::async_trait]
pub trait ScheduleStore: Send + Sync {
    /// `limit` schedules in id order after `after`.
    async fn list_schedules_for_report(
        &self,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<ScheduleReportRow>, MetadataError>;

    /// Records the reason of each of these schedules (the metadata derives it).
    async fn record_schedule_errors(&self, ids: &[Uuid]) -> Result<(), MetadataError>;
}

#[async_trait::async_trait]
impl ScheduleStore for MetadataClient {
    async fn list_schedules_for_report(
        &self,
        after: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<ScheduleReportRow>, MetadataError> {
        MetadataClient::list_schedules_for_report(self, after, limit).await
    }

    async fn record_schedule_errors(&self, ids: &[Uuid]) -> Result<(), MetadataError> {
        MetadataClient::record_schedule_errors(self, ids).await
    }
}

/// Checks every stored schedule once, logs the ones that don't parse and records their reasons.
/// Returns how many it rejected (`None` when the read failed before it finished). Awaited
/// before the schedule loop is spawned; it never fails the worker.
pub async fn run(store: &dyn ScheduleStore) -> Option<usize> {
    let (mut checked, mut rejected) = (0usize, 0usize);
    let mut after = None;
    loop {
        let page = match store.list_schedules_for_report(after, PAGE).await {
            Ok(page) => page,
            Err(e) => {
                error!(checked, rejected, "the schedule report could not run: reading the stored schedules failed: {e}");
                return None;
            }
        };
        let Some(last) = page.last() else { break };
        after = Some(last.id);
        let full = page.len() as i64 == PAGE;
        let (mut failing, mut to_record) = (0usize, Vec::new());
        for row in &page {
            if let Err(why) = Schedule::parse(&row.cron, &row.timezone) {
                warn!(
                    schedule = %row.id,
                    branch_id = %row.branch_id,
                    function = %row.function_name,
                    cron = ?row.cron,
                    timezone = ?row.timezone,
                    enabled = row.enabled,
                    "stored schedule no longer parses and will be switched off when it comes due: {why}"
                );
                failing += 1;
                if row.enabled {
                    to_record.push(row.id);
                }
            }
        }
        checked += page.len();
        rejected += failing;
        if !to_record.is_empty()
            && let Err(e) = store.record_schedule_errors(&to_record).await
        {
            error!(count = to_record.len(), "the schedule report could not record the reasons in last_error: {e}");
        }
        if !full {
            break;
        }
    }
    info!(checked, rejected, "schedule report: checked stored schedules against the current cron parser");
    Some(rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::FakeSchedules;
    use chelabase_types::BranchId;

    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
        type Writer = Logs;
        fn make_writer(&'a self) -> Logs {
            Logs(self.0.clone())
        }
    }

    impl Captured {
        fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
            crate::fakes::pin_tracing_dispatchers();
            let logs = Captured::default();
            let subscriber = tracing_subscriber::fmt()
                .with_writer(logs.clone())
                .with_ansi(false)
                .with_max_level(tracing::Level::TRACE)
                .finish();
            (logs, tracing::subscriber::set_default(subscriber))
        }

        fn lines(&self, level: &str) -> Vec<String> {
            String::from_utf8_lossy(&self.0.lock().unwrap())
                .lines()
                .filter(|l| l.contains(level))
                .map(str::to_owned)
                .collect()
        }
    }

    fn row(cron: &str, timezone: &str, enabled: bool) -> ScheduleReportRow {
        ScheduleReportRow {
            id: Uuid::new_v4(),
            branch_id: BranchId(Uuid::new_v4()),
            function_name: "nightly".into(),
            cron: cron.into(),
            timezone: timezone.into(),
            enabled,
        }
    }

    #[tokio::test]
    async fn lists_each_schedule_whose_cron_stopped_parsing() {
        let (logs, _g) = Captured::capture();
        let (a, b, c) = (row("0 0 * * jan", "UTC", true), row("0 0 * * L", "UTC", true), row("*/5 * * * *", "UTC", true));
        let store = FakeSchedules::new(vec![a.clone(), b.clone(), c.clone()]);
        assert_eq!(run(&store).await, Some(2));
        let warns = logs.lines("WARN");
        assert_eq!(warns.len(), 2, "{warns:?}");
        let mut bad = [&a, &b];
        bad.sort_by_key(|r| r.id);
        for (w, r) in warns.iter().zip(bad) {
            for needle in [r.id.to_string(), r.branch_id.to_string(), "nightly".into(), format!("cron=\"{}\"", r.cron), "timezone=\"UTC\"".into(), "enabled=true".into(), "cron: not a 5-field cron".into()] {
                assert!(w.contains(&needle), "{needle} in {w}");
            }
        }
        assert!(warns.iter().all(|w| !w.contains(&c.id.to_string())));
        let infos = logs.lines("INFO");
        assert_eq!(infos.len(), 1, "{infos:?}");
        assert!(infos[0].contains("rejected=2"), "{infos:?}");
    }

    #[tokio::test]
    async fn lists_disabled_schedules_too() {
        let (logs, _g) = Captured::capture();
        let store = FakeSchedules::new(vec![row("0 0 * * jan", "UTC", false)]);
        assert_eq!(run(&store).await, Some(1));
        let warns = logs.lines("WARN");
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("enabled=false"), "{warns:?}");
    }

    #[tokio::test]
    async fn lists_a_schedule_whose_time_zone_stopped_parsing() {
        let (logs, _g) = Captured::capture();
        let store = FakeSchedules::new(vec![row("0 0 * * *", "Mars/Olympus", true)]);
        assert_eq!(run(&store).await, Some(1));
        let warns = logs.lines("WARN");
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("timezone: unknown IANA time zone"), "{warns:?}");
    }

    #[tokio::test]
    async fn says_zero_when_nothing_is_affected() {
        let (logs, _g) = Captured::capture();
        let store = FakeSchedules::new(vec![row("*/5 * * * *", "UTC", true)]);
        assert_eq!(run(&store).await, Some(0));
        assert!(logs.lines("WARN").is_empty());
        let infos = logs.lines("INFO");
        assert_eq!(infos.len(), 1, "{infos:?}");
        assert!(infos[0].contains("rejected=0"), "{infos:?}");
        assert!(store.recorded().is_empty(), "nothing to record: no write call for no ids");
    }

    #[tokio::test]
    async fn records_only_the_failing_enabled_ids() {
        let (_logs, _g) = Captured::capture();
        let (a, b, c) = (row("0 0 * * jan", "UTC", true), row("*/5 * * * *", "UTC", true), row("0 0 * * *", "Nope/Zone", false));
        let d = row("0 0 * * L", "UTC", true);
        let store = FakeSchedules::new(vec![a.clone(), b, c, d.clone()]);
        run(&store).await;
        let mut want = vec![a.id, d.id];
        want.sort();
        assert_eq!(store.recorded(), vec![want], "in id order, one call, enabled ones only");
        assert_eq!(store.reads(), 1, "the report only reads and records");
    }

    #[tokio::test]
    async fn logs_a_failing_disabled_schedule_but_records_only_the_enabled_one() {
        let (logs, _g) = Captured::capture();
        let (on, off) = (row("0 0 * * jan", "UTC", true), row("0 0 * * L", "UTC", false));
        let store = FakeSchedules::new(vec![on.clone(), off.clone()]);
        assert_eq!(run(&store).await, Some(2), "both are counted as rejected");
        let warns = logs.lines("WARN");
        assert_eq!(warns.len(), 2, "{warns:?}");
        for id in [on.id, off.id] {
            assert!(warns.iter().any(|w| w.contains(&id.to_string())), "{id} logged: {warns:?}");
        }
        assert_eq!(store.recorded(), vec![vec![on.id]], "the disabled one never gets a last_error");
    }

    #[tokio::test]
    async fn a_failed_write_still_logs_and_does_not_stop_the_worker() {
        let (logs, _g) = Captured::capture();
        let store = FakeSchedules::new(vec![row("0 0 * * jan", "UTC", true)]);
        store.fail_writes();
        assert_eq!(run(&store).await, Some(1));
        assert_eq!(logs.lines("WARN").len(), 1);
        let errors = logs.lines("ERROR");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("record"), "{errors:?}");
        assert!(logs.lines("INFO").iter().any(|l| l.contains("rejected=1")));
    }

    #[tokio::test]
    async fn a_failed_read_is_logged_and_does_not_stop_the_worker() {
        let (logs, _g) = Captured::capture();
        let store = FakeSchedules::new(vec![row("0 0 * * jan", "UTC", true)]);
        store.fail_reads();
        assert_eq!(run(&store).await, None);
        let errors = logs.lines("ERROR");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("could not run"), "{errors:?}");
        assert!(logs.lines("INFO").is_empty());
    }

    #[tokio::test]
    async fn checks_every_page_once() {
        let (logs, _g) = Captured::capture();
        let n = PAGE as usize * 2 + 7;
        let rows: Vec<_> = (0..n).map(|i| if i % 100 == 0 { row("0 0 * * jan", "UTC", true) } else { row("* * * * *", "UTC", true) }).collect();
        let bad = rows.iter().filter(|r| r.cron.ends_with("jan")).count();
        let store = FakeSchedules::new(rows);
        assert_eq!(run(&store).await, Some(bad));
        assert_eq!(store.seen(), n, "each schedule read once");
        assert_eq!(store.reads(), 3, "two full pages and a short one");
        assert_eq!(logs.lines("WARN").len(), bad);
        assert_eq!(store.recorded().iter().map(Vec::len).sum::<usize>(), bad);
    }
}
