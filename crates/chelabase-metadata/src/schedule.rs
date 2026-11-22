//! Cron schedules: parsing a 5-field cron in an IANA time zone, the next tick after an instant,
//! and what a claim does with a due schedule ([`advance`]).

use chrono::{DateTime, Utc};
use tracing::warn;

/// A schedule's part a caller got wrong ([`Schedule::parse`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleError {
    Cron(String),
    Timezone(String),
}

impl ScheduleError {
    /// The request field it names: `cron` or `timezone`.
    pub fn field(&self) -> &'static str {
        match self {
            ScheduleError::Cron(_) => "cron",
            ScheduleError::Timezone(_) => "timezone",
        }
    }
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScheduleError::Cron(why) | ScheduleError::Timezone(why) => {
                write!(f, "{}: {why}", self.field())
            }
        }
    }
}

/// A checked schedule: a 5-field cron (minute granularity, no seconds or years) in an IANA
/// time zone. Times in a DST gap run at the first instant after it; times a fall-back repeats
/// run once.
#[derive(Debug, Clone)]
pub struct Schedule {
    cron: croner::Cron,
    tz: chrono_tz::Tz,
}

impl Schedule {
    pub fn parse(cron: &str, timezone: &str) -> Result<Schedule, ScheduleError> {
        use croner::parser::{CronParser, Seconds, Year};
        if cron.is_empty() || cron.len() > 100 {
            return Err(ScheduleError::Cron("1 to 100 characters".into()));
        }
        if timezone.is_empty() || timezone.len() > 64 {
            return Err(ScheduleError::Timezone("1 to 64 characters".into()));
        }
        let parsed = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .year(Year::Disallowed)
            .build()
            .parse(cron)
            .map_err(|e| ScheduleError::Cron(format!("not a 5-field cron ({e})")))?;
        let tz = timezone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| ScheduleError::Timezone(format!("unknown IANA time zone {timezone:?}")))?;
        Ok(Schedule { cron: parsed, tz })
    }

    /// The first time strictly after `after`; `None` when there is none (a date that never
    /// comes) or the search failed (logged with the cron and zone, [`Schedule::try_next_after`]
    /// tells the two apart).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self.try_next_after(after) {
            Ok(next) => next,
            Err(e) => {
                warn!(cron = %self.cron.pattern, timezone = %self.tz, error = %e, "schedule search failed; treating the schedule as never firing");
                None
            }
        }
    }

    /// Like [`Schedule::next_after`]: `Ok(None)` when the cron never fires again (croner's
    /// search limit), `Err` when the search itself failed.
    pub fn try_next_after(
        &self,
        after: DateTime<Utc>,
    ) -> Result<Option<DateTime<Utc>>, croner::errors::CronError> {
        never_fires_is_none(
            self.cron
                .find_next_occurrence(&after.with_timezone(&self.tz), false),
        )
        .map(|t| t.map(|t| t.with_timezone(&Utc)))
    }
}

/// Only croner's search limit means "no occurrence"; any other error is a failure.
fn never_fires_is_none<T>(
    found: Result<T, croner::errors::CronError>,
) -> Result<Option<T>, croner::errors::CronError> {
    match found {
        Ok(t) => Ok(Some(t)),
        Err(croner::errors::CronError::TimeSearchLimitExceeded) => Ok(None),
        Err(e) => Err(e),
    }
}

/// What a claim does with one due schedule ([`advance`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advance {
    /// The new `next_run_at`.
    pub next: Option<DateTime<Utc>>,
    /// Run the function for this tick (the `scheduledTime`): the due `next_run_at`, once, however
    /// many ticks were missed; `None` for a disabled or undeployed function, or a schedule that
    /// had no time yet.
    pub run: Option<DateTime<Utc>>,
    /// The schedule stays enabled; false when its cron or time zone no longer parses or it has
    /// no time left.
    pub enabled: bool,
}

/// A due schedule's step at `now`: missed ticks coalesce into one run, and `next_run_at` moves
/// to the first tick after `now`, whether the function runs or not.
pub fn advance(
    cron: &str,
    timezone: &str,
    due: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    function_enabled: bool,
) -> Advance {
    let next = Schedule::parse(cron, timezone)
        .ok()
        .and_then(|s| s.next_after(now));
    Advance {
        next,
        run: due.filter(|_| function_enabled && next.is_some()),
        enabled: next.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn rejects_names_outside_their_field() {
        // croner 4.0.2 parsed these and ran a schedule nobody wrote.
        for cron in ["0 0 * * jan", "mon * * * *", "0 0 mon * *", "0 0 * mon *"] {
            assert!(
                matches!(Schedule::parse(cron, "UTC"), Err(ScheduleError::Cron(_))),
                "{cron} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_a_bare_l_in_day_of_week() {
        let err = Schedule::parse("0 0 * * L", "UTC").unwrap_err();
        assert!(matches!(err, ScheduleError::Cron(_)), "{err:?}");
        assert_eq!(err.field(), "cron");
    }

    #[test]
    fn keeps_accepting_names_in_their_own_field() {
        for cron in [
            "0 0 * * MON-FRI",
            "0 0 1 JAN *",
            "0 0 * * 5L",
            "*/5 9-17 * * MON-FRI",
        ] {
            Schedule::parse(cron, "Europe/London").unwrap_or_else(|e| panic!("{cron}: {e}"));
        }
    }

    #[test]
    fn runs_once_on_the_day_clocks_go_forward() {
        // 2026-03-29: Europe/London skips 01:00-02:00. A midnight job runs once that day.
        let s = Schedule::parse("0 0 * * *", "Europe/London").unwrap();
        let mut t = at("2026-03-27T12:00:00Z");
        let mut runs = Vec::new();
        for _ in 0..4 {
            t = s.next_after(t).unwrap();
            runs.push(t);
        }
        assert_eq!(
            runs,
            [
                at("2026-03-28T00:00:00Z"),
                at("2026-03-29T00:00:00Z"),
                at("2026-03-29T23:00:00Z"),
                at("2026-03-30T23:00:00Z"),
            ]
        );
    }

    #[test]
    fn next_after_a_tick_is_strictly_later() {
        let s = Schedule::parse("*/15 * * * *", "UTC").unwrap();
        let tick = at("2026-06-01T10:15:00Z");
        assert_eq!(s.next_after(tick), Some(at("2026-06-01T10:30:00Z")));
        let z = Schedule::parse("0 9 * * *", "Europe/London").unwrap();
        let tick = at("2026-06-01T08:00:00Z");
        assert_eq!(z.next_after(tick), Some(at("2026-06-02T08:00:00Z")));
    }

    #[test]
    fn a_search_error_is_not_reported_as_never_fires() {
        use croner::errors::CronError;
        assert!(matches!(never_fires_is_none::<u8>(Ok(1)), Ok(Some(1))));
        assert!(matches!(
            never_fires_is_none::<u8>(Err(CronError::TimeSearchLimitExceeded)),
            Ok(None)
        ));
        assert!(matches!(
            never_fires_is_none::<u8>(Err(CronError::InvalidTime)),
            Err(CronError::InvalidTime)
        ));
        // A real never-fires cron is Ok(None) through the public path.
        let s = Schedule::parse("0 0 30 2 *", "UTC").unwrap();
        assert!(matches!(
            s.try_next_after(at("2026-01-01T00:00:00Z")),
            Ok(None)
        ));
    }

    #[test]
    fn advance_disables_a_schedule_whose_cron_does_not_parse() {
        let now = at("2026-06-01T10:00:00Z");
        let step = advance("0 0 * * jan", "UTC", Some(now), now, true);
        assert_eq!(
            step,
            Advance {
                next: None,
                run: None,
                enabled: false
            }
        );
    }
}
