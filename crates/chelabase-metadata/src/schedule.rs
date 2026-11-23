//! Cron schedules: parsing a 5-field cron in an IANA time zone, the next tick after an instant,
//! and what a claim does with a due schedule ([`advance`]).
//!
//! # The invariant
//!
//! A schedule runs once per wall-clock time its cron matches in its zone. A matching time
//! inside a gap (clocks go forward) runs at the first instant after the gap; a matching time a
//! fall-back repeats runs once, at its first occurrence. The next time is always strictly
//! after the instant asked about.
//!
//! The times come from croner's jiff backend in jiff's bundled tz database, never the host's
//! zoneinfo. A zone name is accepted only as the database spells it, byte for byte (exact
//! case, no aliasing by case, no POSIX or offset strings). Until croner's chrono backend and
//! `chrono-tz` left the tree, a model test compared every next time with theirs on random
//! crons, zones and instants and at every DST transition of a few awkward zones;
//! `next_runs_goldens` keeps the outputs both agreed on, and the zone-name fixture keeps
//! `chrono-tz` 0.10.4's list. jiff's bundled tzdata (2026f) is the accepted source since.

use chelabase_time::{Timestamp, TimestampExt};
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

/// Names jiff's bundle has that `chrono-tz` 0.10.4 refused, refused still: `Factory` is the
/// tz database's placeholder for "no zone set", not a place.
const REFUSED_ZONES: &[&str] = &["Factory"];

/// A checked schedule: a 5-field cron (minute granularity, no seconds or years) in an IANA
/// time zone. Times in a DST gap run at the first instant after it; times a fall-back repeats
/// run once.
#[derive(Debug, Clone)]
pub struct Schedule {
    cron: croner::Cron,
    tz: jiff::tz::TimeZone,
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
        let tz = bundled_zone(timezone)
            .ok_or_else(|| ScheduleError::Timezone(format!("unknown IANA time zone {timezone:?}")))?;
        Ok(Schedule { cron: parsed, tz })
    }

    /// The first time strictly after `after`; `None` when there is none (a date that never
    /// comes) or the search failed (logged with the cron and zone, [`Schedule::try_next_after`]
    /// tells the two apart).
    pub fn next_after(&self, after: Timestamp) -> Option<Timestamp> {
        match self.try_next_after(after) {
            Ok(next) => next,
            Err(e) => {
                warn!(cron = %self.cron.pattern, timezone = self.zone_name(), error = %e, "schedule search failed; treating the schedule as never firing");
                None
            }
        }
    }

    /// Like [`Schedule::next_after`]: `Ok(None)` when the cron never fires again (croner's
    /// search limit), `Err` with the reason when the search itself failed (croner's other
    /// errors, or an instant outside jiff's range).
    pub fn try_next_after(&self, after: Timestamp) -> Result<Option<Timestamp>, String> {
        let start = jiff::Timestamp::from_nanosecond(after.unix_nanos())
            .map_err(|e| format!("{} is out of range ({e})", after.to_rfc3339_auto()))?
            .to_zoned_datetime(self.tz.clone());
        let found = never_fires_is_none(self.cron.find_next_occurrence(&start, false))
            .map_err(|e| e.to_string())?;
        found
            .map(|next| {
                let ns = next.timestamp().as_nanosecond();
                from_unix_nanos(ns).ok_or_else(|| format!("{} is out of range", next.timestamp()))
            })
            .transpose()
    }

    fn zone_name(&self) -> &str {
        self.tz.iana_name().unwrap_or("?")
    }
}

/// `name` in jiff's bundled tz database, only when that is exactly how the database spells
/// it: its lookup ignores ASCII case, and the stored name has to keep round-tripping.
fn bundled_zone(name: &str) -> Option<jiff::tz::TimeZone> {
    if REFUSED_ZONES.contains(&name) {
        return None;
    }
    let tz = jiff::tz::TimeZoneDatabase::bundled().get(name).ok()?;
    (tz.iana_name() == Some(name)).then_some(tz)
}

/// The instant `ns` nanoseconds after the Unix epoch, in whichever type backs [`Timestamp`].
fn from_unix_nanos(ns: i128) -> Option<Timestamp> {
    const NS: i128 = 1_000_000_000;
    let secs = i64::try_from(ns.div_euclid(NS)).ok()?;
    let sub = u64::try_from(ns.rem_euclid(NS)).ok()?;
    Timestamp::from_unix_seconds(secs)?.checked_add(std::time::Duration::from_nanos(sub))
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
    pub next: Option<Timestamp>,
    /// Run the function for this tick (the `scheduledTime`): the due `next_run_at`, once, however
    /// many ticks were missed; `None` for a disabled or undeployed function, or a schedule that
    /// had no time yet.
    pub run: Option<Timestamp>,
    /// The schedule stays enabled; false when its cron or time zone no longer parses or it has
    /// no time left.
    pub enabled: bool,
    /// Why the schedule was switched off (`enabled` false), for `last_error`: the parser's own
    /// message or `no upcoming time`, at most [`SCHEDULE_ERROR_MAX_CHARS`] characters; `None` when
    /// the schedule parsed and has a next time.
    pub error: Option<String>,
}

/// The longest `last_error` the column holds (its CHECK counts characters).
pub const SCHEDULE_ERROR_MAX_CHARS: usize = 500;

/// What `last_error` says for a schedule that parsed but has no time left.
pub const NO_UPCOMING_TIME: &str = "no upcoming time";

/// Why a stored schedule can't run: the parser's typed error, a cron that parses but never
/// fires again, or a search for its next time that failed. Only [`next_run`] makes one from a
/// stored cron and time zone, and it is the only source of `last_error` text: a claim
/// ([`advance`]) and the startup report ([`crate::MetadataClient::record_schedule_errors`])
/// both derive it from the row itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScheduleProblem {
    Invalid(ScheduleError),
    NoUpcomingTime,
    /// The search error's text (croner's, or an instant outside the supported range).
    SearchFailed(String),
}

impl ScheduleProblem {
    /// What `last_error` stores: never empty, at most [`SCHEDULE_ERROR_MAX_CHARS`] characters (cut
    /// on a character boundary, as Postgres `length()` counts them).
    pub fn reason(&self) -> String {
        let text = match self {
            ScheduleProblem::Invalid(e) => e.to_string(),
            ScheduleProblem::NoUpcomingTime => NO_UPCOMING_TIME.to_owned(),
            ScheduleProblem::SearchFailed(e) => format!("cron: the next time could not be computed ({e})"),
        };
        text.chars().take(SCHEDULE_ERROR_MAX_CHARS).collect()
    }
}

/// A stored schedule's first time strictly after `now`, or why it has none.
pub fn next_run(cron: &str, timezone: &str, now: Timestamp) -> Result<Timestamp, ScheduleProblem> {
    let schedule = Schedule::parse(cron, timezone).map_err(ScheduleProblem::Invalid)?;
    match schedule.try_next_after(now) {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(ScheduleProblem::NoUpcomingTime),
        Err(e) => {
            warn!(cron, timezone, error = %e, "schedule search failed");
            Err(ScheduleProblem::SearchFailed(e))
        }
    }
}

/// A due schedule's step at `now`: missed ticks coalesce into one run, and `next_run_at` moves
/// to the first tick after `now`, whether the function runs or not.
pub fn advance(
    cron: &str,
    timezone: &str,
    due: Option<Timestamp>,
    now: Timestamp,
    function_enabled: bool,
) -> Advance {
    let (next, problem) = match next_run(cron, timezone, now) {
        Ok(t) => (Some(t), None),
        Err(p) => (None, Some(p)),
    };
    Advance {
        next,
        run: due.filter(|_| function_enabled && next.is_some()),
        enabled: next.is_some(),
        error: problem.map(|p| p.reason()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Timestamp {
        Timestamp::parse_rfc3339(s).unwrap()
    }

    /// The zone names `chrono-tz` 0.10.4 accepted (its `TZ_VARIANTS`), frozen.
    fn chrono_tz_names() -> Vec<&'static str> {
        include_str!("../tests/fixtures/tz_names_chrono_tz_0_10_4.txt")
            .lines()
            .collect()
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
                enabled: false,
                error: Some(
                    Schedule::parse("0 0 * * jan", "UTC").unwrap_err().to_string()
                ),
            }
        );
    }

    #[test]
    fn advance_reports_the_reason_it_disabled() {
        let now = at("2026-06-01T10:00:00Z");
        let why = |cron: &str, tz: &str| advance(cron, tz, Some(now), now, true).error;
        assert!(why("0 0 * * jan", "UTC").unwrap().starts_with("cron: "));
        assert!(why("* * * * *", "Mars/Olympus").unwrap().starts_with("timezone: "));
        assert_eq!(why("0 0 30 2 *", "UTC").as_deref(), Some("no upcoming time"));
        assert_eq!(why("* * * * *", "UTC"), None, "a healthy schedule has no reason");
        // A function that is off doesn't make a healthy schedule an error.
        assert_eq!(advance("* * * * *", "UTC", Some(now), now, false).error, None);
    }

    #[test]
    fn a_reason_is_cut_to_500_characters_on_a_boundary() {
        let long = ScheduleProblem::Invalid(ScheduleError::Timezone("é".repeat(900)));
        let text = long.reason();
        assert_eq!(text.chars().count(), SCHEDULE_ERROR_MAX_CHARS);
        assert!(text.starts_with("timezone: éé"));
        assert_eq!(ScheduleProblem::NoUpcomingTime.reason(), "no upcoming time");
    }

    #[test]
    fn a_search_failure_has_its_own_reason() {
        // Audit N2: a croner failure other than its search limit isn't "no upcoming time".
        let why = ScheduleProblem::SearchFailed(croner::errors::CronError::InvalidTime.to_string()).reason();
        assert!(why.starts_with("cron: the next time could not be computed ("), "{why}");
        assert_ne!(why, NO_UPCOMING_TIME);
    }

    #[test]
    fn next_run_derives_the_reason_from_the_schedule() {
        let now = at("2026-06-01T10:00:00Z");
        assert_eq!(next_run("*/15 * * * *", "UTC", now), Ok(at("2026-06-01T10:15:00Z")));
        assert_eq!(next_run("0 0 30 2 *", "UTC", now), Err(ScheduleProblem::NoUpcomingTime));
        assert_eq!(
            next_run("0 0 * * jan", "UTC", now),
            Err(ScheduleProblem::Invalid(Schedule::parse("0 0 * * jan", "UTC").unwrap_err()))
        );
        assert!(matches!(next_run("* * * * *", "Mars/Olympus", now), Err(ScheduleProblem::Invalid(ScheduleError::Timezone(_)))));
    }

    #[test]
    fn every_zone_chrono_tz_knew_still_parses() {
        let names = chrono_tz_names();
        assert_eq!(names.len(), 597, "the fixture is chrono-tz 0.10.4's whole list");
        let refused: Vec<_> = names
            .iter()
            .filter(|name| Schedule::parse("0 0 * * *", name).is_err())
            .collect();
        assert!(refused.is_empty(), "zones chrono-tz accepted, refused now: {refused:?}");
    }

    #[test]
    fn the_bundle_adds_only_names_that_stay_refused() {
        // A tzdb update that adds a zone fails here, so accepting it is a decision.
        let known: std::collections::BTreeSet<_> = chrono_tz_names().into_iter().collect();
        let extra: Vec<String> = jiff::tz::TimeZoneDatabase::bundled()
            .available()
            .map(|n| n.as_str().to_owned())
            .filter(|n| !known.contains(n.as_str()))
            .collect();
        assert_eq!(extra, REFUSED_ZONES, "names jiff's bundle has beyond chrono-tz 0.10.4");
        for name in REFUSED_ZONES {
            assert!(matches!(Schedule::parse("0 0 * * *", name), Err(ScheduleError::Timezone(_))), "{name}");
        }
    }

    #[test]
    fn zone_names_are_case_sensitive_and_iana_only() {
        for tz in [
            "",
            "Mars/Olympus",
            "europe/london",
            "EUROPE/LONDON",
            "Europe/london",
            "utc",
            "etc/utc",
            "UTC+1",
            "+01:00",
            "Z",
            "EST5EDT,M3.2.0,M11.1.0",
            "<+03>-3",
            "Etc/Unknown",
            "Factory",
            "/usr/share/zoneinfo/Europe/London",
            " Europe/London",
            &format!("America/{}", "A".repeat(57)),
        ] {
            let err = Schedule::parse("0 0 * * *", tz).map(|_| ()).unwrap_err();
            assert!(matches!(err, ScheduleError::Timezone(_)), "{tz:?}: {err:?}");
            assert!(err.to_string().starts_with("timezone: "), "{err}");
        }
        assert_eq!("America/".len() + 57, 65);
        for tz in ["UTC", "Etc/UTC", "Europe/London", "US/Eastern", "GB", "EST5EDT", "Etc/GMT+5"] {
            Schedule::parse("0 0 * * *", tz).unwrap_or_else(|e| panic!("{tz}: {e}"));
        }
    }

    #[test]
    fn parses_named_zones_without_system_zoneinfo() {
        // An empty TZDIR: a lookup that read the host's zoneinfo would find nothing. Each test
        // is its own process under nextest, so the variable stays with this test.
        let empty = std::env::temp_dir().join(format!("chelabase-empty-tzdir-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        // SAFETY: set before this test starts any thread that reads the environment.
        unsafe { std::env::set_var("TZDIR", &empty) };
        let s = Schedule::parse("0 9 * * *", "Europe/London").unwrap();
        assert_eq!(s.next_after(at("2026-06-01T09:00:00Z")), Some(at("2026-06-02T08:00:00Z")));
        let _ = std::fs::remove_dir(&empty);
    }

    #[test]
    fn a_cron_that_never_fires_has_no_next_run() {
        let s = Schedule::parse("0 0 30 2 *", "Europe/London").unwrap();
        let started = std::time::Instant::now();
        assert_eq!(s.try_next_after(at("2026-01-01T00:00:00Z")), Ok(None));
        assert_eq!(s.next_after(at("2026-01-01T00:00:00Z")), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
        let now = at("2026-06-01T10:00:00Z");
        let step = advance("0 0 30 2 *", "Europe/London", Some(now), now, true);
        assert_eq!((step.next, step.run, step.enabled), (None, None, false));
        assert_eq!(step.error.as_deref(), Some(NO_UPCOMING_TIME));
    }

    #[test]
    fn a_zone_the_bundle_refuses_is_reported_as_a_timezone_error() {
        let now = at("2026-06-01T10:00:00Z");
        for tz in ["Factory", "europe/london"] {
            let why = next_run("* * * * *", tz, now).unwrap_err().reason();
            assert!(why.starts_with("timezone: "), "{tz}: {why}");
            let step = advance("* * * * *", tz, Some(now), now, true);
            assert!(!step.enabled && step.error.as_deref().is_some_and(|e| e.starts_with("timezone: ")), "{step:?}");
        }
    }

    #[test]
    fn keeps_sub_second_instants_and_the_strictly_after_rule() {
        let s = Schedule::parse("* * * * *", "UTC").unwrap();
        assert_eq!(s.next_after(at("2026-01-01T00:00:59.999999999Z")), Some(at("2026-01-01T00:01:00Z")));
        assert_eq!(s.next_after(at("1969-12-31T23:59:30.5Z")), Some(at("1970-01-01T00:00:00Z")));
        assert_eq!(from_unix_nanos(-1), Some(at("1969-12-31T23:59:59.999999999Z")));
    }

    /// (cron, zone, start) and the next three runs both backends agreed on before croner's
    /// chrono backend went (m11): the record of that agreement.
    const GOLDENS: &[(&str, &str, &str, [&str; 3])] = &[
        ("30 1 * * *", "Europe/London", "2026-03-28T00:00:00Z", ["2026-03-28T01:30:00Z", "2026-03-29T01:00:00Z", "2026-03-30T00:30:00Z"]),
        ("30 1 * * *", "Europe/London", "2026-10-24T12:00:00Z", ["2026-10-25T00:30:00Z", "2026-10-26T01:30:00Z", "2026-10-27T01:30:00Z"]),
        ("0 9 * * MON-FRI", "Europe/London", "2026-03-27T12:00:00Z", ["2026-03-30T08:00:00Z", "2026-03-31T08:00:00Z", "2026-04-01T08:00:00Z"]),
        ("*/15 1-2 * * *", "Europe/London", "2026-10-25T00:20:00Z", ["2026-10-25T00:30:00Z", "2026-10-25T00:45:00Z", "2026-10-25T01:00:00Z"]),
        ("30 2 * * *", "America/New_York", "2026-03-07T12:00:00Z", ["2026-03-08T07:00:00Z", "2026-03-09T06:30:00Z", "2026-03-10T06:30:00Z"]),
        ("30 1 * * *", "America/New_York", "2026-10-31T12:00:00Z", ["2026-11-01T05:30:00Z", "2026-11-02T06:30:00Z", "2026-11-03T06:30:00Z"]),
        ("0 * * * *", "America/New_York", "2026-11-01T04:30:00Z", ["2026-11-01T05:00:00Z", "2026-11-01T06:00:00Z", "2026-11-01T07:00:00Z"]),
        ("0 0 1 * *", "America/New_York", "2026-01-15T00:00:00Z", ["2026-02-01T05:00:00Z", "2026-03-01T05:00:00Z", "2026-04-01T04:00:00Z"]),
        ("0 2 * * *", "Australia/Lord_Howe", "2026-04-04T12:00:00Z", ["2026-04-04T15:30:00Z", "2026-04-05T15:30:00Z", "2026-04-06T15:30:00Z"]),
        ("15 2 * * *", "Australia/Lord_Howe", "2026-10-03T12:00:00Z", ["2026-10-03T15:30:00Z", "2026-10-04T15:15:00Z", "2026-10-05T15:15:00Z"]),
        ("*/20 1-2 * * SUN", "Australia/Lord_Howe", "2026-04-04T13:00:00Z", ["2026-04-04T14:00:00Z", "2026-04-04T14:20:00Z", "2026-04-04T14:40:00Z"]),
        ("0 0 * * *", "Pacific/Apia", "2011-12-28T00:00:00Z", ["2011-12-28T10:00:00Z", "2011-12-29T10:00:00Z", "2011-12-30T10:00:00Z"]),
        ("0 12 30 12 *", "Pacific/Apia", "2011-12-01T00:00:00Z", ["2012-12-29T22:00:00Z", "2013-12-29T22:00:00Z", "2014-12-29T22:00:00Z"]),
        ("30 3 * * *", "Pacific/Apia", "2021-04-03T12:00:00Z", ["2021-04-03T13:30:00Z", "2021-04-04T14:30:00Z", "2021-04-05T14:30:00Z"]),
        ("0 * * * *", "America/Nuuk", "2023-03-25T00:00:00Z", ["2023-03-25T01:00:00Z", "2023-03-25T02:00:00Z", "2023-03-25T03:00:00Z"]),
        ("30 22 * * SAT", "America/Nuuk", "2024-10-25T12:00:00Z", ["2024-10-26T23:30:00Z", "2024-11-03T00:30:00Z", "2024-11-10T00:30:00Z"]),
        ("0 0 * * *", "America/Nuuk", "2024-03-29T12:00:00Z", ["2024-03-30T02:00:00Z", "2024-03-31T01:00:00Z", "2024-04-01T01:00:00Z"]),
        ("0 9 * * *", "Asia/Kolkata", "2026-06-01T00:00:00Z", ["2026-06-01T03:30:00Z", "2026-06-02T03:30:00Z", "2026-06-03T03:30:00Z"]),
        ("45 23 L * *", "Asia/Kathmandu", "2026-02-01T00:00:00Z", ["2026-02-28T18:00:00Z", "2026-03-31T18:00:00Z", "2026-04-30T18:00:00Z"]),
        ("* * * * *", "UTC", "2026-12-31T23:58:30.250Z", ["2026-12-31T23:59:00Z", "2027-01-01T00:00:00Z", "2027-01-01T00:01:00Z"]),
        ("0 0 29 2 *", "UTC", "2026-01-01T00:00:00Z", ["2028-02-29T00:00:00Z", "2032-02-29T00:00:00Z", "2036-02-29T00:00:00Z"]),
        ("0 12 * * *", "Etc/UTC", "2030-07-04T13:00:00Z", ["2030-07-05T12:00:00Z", "2030-07-06T12:00:00Z", "2030-07-07T12:00:00Z"]),
        ("0 6 * * *", "Etc/GMT+5", "2026-03-08T00:00:00Z", ["2026-03-08T11:00:00Z", "2026-03-09T11:00:00Z", "2026-03-10T11:00:00Z"]),
        ("0 0 * * *", "America/Sao_Paulo", "2018-11-03T12:00:00Z", ["2018-11-04T03:00:00Z", "2018-11-05T02:00:00Z", "2018-11-06T02:00:00Z"]),
        ("0 0 * * *", "America/Sao_Paulo", "2019-02-15T12:00:00Z", ["2019-02-16T02:00:00Z", "2019-02-17T03:00:00Z", "2019-02-18T03:00:00Z"]),
        ("45 2 * * *", "Pacific/Chatham", "2026-09-26T00:00:00Z", ["2026-09-26T14:00:00Z", "2026-09-27T13:00:00Z", "2026-09-28T13:00:00Z"]),
        ("45 3 * * *", "Pacific/Chatham", "2026-04-04T00:00:00Z", ["2026-04-04T15:00:00Z", "2026-04-05T15:00:00Z", "2026-04-06T15:00:00Z"]),
        ("30 1 * * *", "Europe/Dublin", "2026-10-24T12:00:00Z", ["2026-10-25T00:30:00Z", "2026-10-26T01:30:00Z", "2026-10-27T01:30:00Z"]),
        ("0 2 * * *", "Africa/Casablanca", "2026-02-01T00:00:00Z", ["2026-02-01T01:00:00Z", "2026-02-02T01:00:00Z", "2026-02-03T01:00:00Z"]),
        ("0 0 * * *", "America/Santiago", "2026-09-05T00:00:00Z", ["2026-09-05T04:00:00Z", "2026-09-06T04:00:00Z", "2026-09-07T03:00:00Z"]),
        ("30 23 * * *", "America/Santiago", "2026-04-04T12:00:00Z", ["2026-04-05T02:30:00Z", "2026-04-06T03:30:00Z", "2026-04-07T03:30:00Z"]),
        ("0 0 * * *", "Asia/Tehran", "2022-03-20T12:00:00Z", ["2022-03-20T20:30:00Z", "2022-03-21T20:30:00Z", "2022-03-22T19:30:00Z"]),
        ("0 0 1 1 *", "Pacific/Kiritimati", "2026-06-01T00:00:00Z", ["2026-12-31T10:00:00Z", "2027-12-31T10:00:00Z", "2028-12-31T10:00:00Z"]),
        ("30 2 * * *", "Australia/Adelaide", "2026-10-03T12:00:00Z", ["2026-10-03T16:30:00Z", "2026-10-04T16:00:00Z", "2026-10-05T16:00:00Z"]),
        ("30 2 * * *", "Australia/Adelaide", "2026-04-04T12:00:00Z", ["2026-04-04T16:00:00Z", "2026-04-05T17:00:00Z", "2026-04-06T17:00:00Z"]),
        ("1 0 * * *", "America/St_Johns", "2026-11-01T00:00:00Z", ["2026-11-01T02:31:00Z", "2026-11-02T03:31:00Z", "2026-11-03T03:31:00Z"]),
        ("0 3 * * 5L", "Europe/Moscow", "2026-01-01T00:00:00Z", ["2026-01-30T00:00:00Z", "2026-02-27T00:00:00Z", "2026-03-27T00:00:00Z"]),
        ("0 0 * * *", "America/Havana", "2026-03-07T12:00:00Z", ["2026-03-08T05:00:00Z", "2026-03-09T04:00:00Z", "2026-03-10T04:00:00Z"]),
        ("0 1 * * *", "America/Havana", "2025-10-31T12:00:00Z", ["2025-11-01T05:00:00Z", "2025-11-02T06:00:00Z", "2025-11-03T06:00:00Z"]),
        ("0 0 * * 1#2", "Europe/Berlin", "2026-03-01T00:00:00Z", ["2026-03-08T23:00:00Z", "2026-04-12T22:00:00Z", "2026-05-10T22:00:00Z"]),
    ];

    fn three_runs(s: &Schedule, from: Timestamp) -> Vec<Option<Timestamp>> {
        let mut t = Some(from);
        (0..3)
            .map(|_| {
                t = t.and_then(|t| s.try_next_after(t).unwrap());
                t
            })
            .collect()
    }

    #[test]
    fn next_runs_goldens() {
        assert_eq!(GOLDENS.len(), 40);
        for (cron, tz, from, want) in GOLDENS {
            let s = Schedule::parse(cron, tz).unwrap();
            let want: Vec<_> = want.iter().map(|w| Some(at(w))).collect();
            assert_eq!(three_runs(&s, at(from)), want, "{cron} in {tz} from {from}");
        }
    }
}
