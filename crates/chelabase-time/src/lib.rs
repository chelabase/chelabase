//! Timestamps, dates and durations for Chelabase: one place that says how a time is built,
//! compared, written to JSON, to Postgres and into a SQL literal, and parsed back.
//!
//! [`Timestamp`] is an instant in UTC with nanosecond precision; [`Date`] is a calendar date.
//! Both are newtypes over jiff (`jiff::Timestamp`, `jiff::civil::Date`). Until epic drop-chrono
//! they were aliases of chrono's types; their wire formats are frozen by this crate's goldens
//! (`tests/format_goldens.rs`), taken from chrono, and the jiff types write and read exactly
//! those. chrono, chrono-tz and humantime are banned from the tree (`deny.toml`).
//!
//! Durations are [`std::time::Duration`] ([`days`], [`hours`], [`minutes`], [`seconds`] build
//! them in `const`), and the signed difference of two timestamps is a [`SignedDuration`].
//! [`parse_duration`] reads the human-readable durations (`1h 30m`) env vars and the
//! pageserver use, with humantime 2.4.0's grammar.
//!
//! Import the traits to use the methods: `use chelabase_time::{Timestamp, TimestampExt};`
//! (or `use chelabase_time::prelude::*;`).
//!
//! # The contract
//!
//! These inherent methods and impls are chrono's names, kept so call sites didn't change:
//!
//! - `Timestamp::timestamp()` (`i64` seconds), `timestamp_millis()`, `timestamp_micros()`
//!   (`i64`, floor: `1969-12-31T23:59:59.999999Z` is `-1` µs);
//! - `Timestamp::UNIX_EPOCH`, and the range's ends `Timestamp::MIN` and `Timestamp::MAX`;
//! - `Copy`, `Eq`, `Ord` (`min`, `max`, `<`), `Hash`, `Debug`;
//! - `SystemTime::from(t)`;
//! - `Timestamp + Duration` and `Timestamp - Duration` with a `std::time::Duration`
//!   (panic on overflow; [`TimestampExt::checked_add`] doesn't);
//! - `Display`: `2026-09-30 12:00:00 UTC`, fraction as 0/3/6/9 digits
//!   (`2026-09-30 12:00:00.500 UTC`). Anything that reaches SQL, JSON, NATS or a user should
//!   use a named `to_rfc3339_*` function instead;
//! - serde: written as [`TimestampExt::to_rfc3339_auto`]; read as RFC 3339 with any offset,
//!   converted to UTC. `Date` is written and read as `2026-10-01`; its `Display` is the same;
//! - `Date` `Copy`, `Eq`, `Ord`, `Hash`.
//!
//! # The jiff types
//!
//! Their parsers are ports of chrono 0.4.45's, so they accept and reject the same strings with
//! the same error messages (the parity tests that proved it against chrono left with chrono;
//! `tests/parse_literals.rs` keeps their literals).
//!
//! Subtracting two timestamps (`a - b`) gives a [`SignedDuration`], as
//! [`TimestampExt::since`] does: signed, nanosecond-exact (chrono's was a `TimeDelta`).
//!
//! ## Differences from chrono
//!
//! Where jiff can't do what chrono did, the types do this instead (`tests/differences.rs` has a
//! test for each; the Postgres ones are also the decoder tests in `src/jiff_backed/pg.rs` and
//! `tests/pg_live.rs`):
//!
//! - **Range.** jiff's instants run from `-9999-01-02T01:59:59Z` to
//!   `9999-12-30T22:00:00.999999999Z` (chrono's to about ±262,000 years); its dates from
//!   `-9999-01-01` to `9999-12-31`. Outside it, constructors return `None`, parsers and serde
//!   fail with chrono's "input is out of range" ([`ParseError::is_beyond_range`] tells it from
//!   a malformed string), and a Postgres decode returns an error. So
//!   `9999-12-31T23:59:59.999999999Z` (and any instant on 9999-12-31 after 22:00) is an error,
//!   while `0001-01-01T00:00:00Z` is in range. Nothing is clamped.
//! - **Leap seconds.** chrono read `23:59:60` (both parsers) as a leap second. The jiff types
//!   read it as the same instant chrono's `unix_nanos` and Postgres encode gave it, the next
//!   minute's second 0 (`23:59:60.5Z` is `00:00:00.5Z`), so it writes that back instead of `:60`
//!   and compares equal to it.
//! - **Postgres `infinity`.** A binary `infinity` made chrono's decode panic on overflow; the jiff
//!   types return an error, in both protocols, for `timestamptz` and `date`.
//! - **Postgres text with an offset in seconds.** Postgres prints local mean time with seconds
//!   (`1900-01-01 05:41:16+05:41:16` in `Asia/Kathmandu`), which chrono's text decode refused;
//!   the jiff types read it.
//! - **Postgres type names.** sqlx keeps its `timestamptz`/`date` constants private, so the type
//!   infos are built from the OIDs (1184, 1185, 1082, 1182). They bind the same parameter types
//!   and compare equal to sqlx's, but `TypeInfo::name()` is `?`, which shows in sqlx's
//!   type-mismatch error messages.
//! - **`Debug`** is the `to_rfc3339_auto` form (chrono's today too), but isn't a contract.
//!   `Hash` values differ from chrono's.
//!
//! # Old call → new call
//!
//! Every chrono and humantime use in `crates/`, `services/` and `tools/` at the time this crate
//! was added, and what replaced it:
//!
//! | Old (chrono / humantime) | New (`chelabase_time`) |
//! |---|---|
//! | `chrono::DateTime<chrono::Utc>`, `DateTime<Utc>` | `Timestamp` |
//! | `chrono::NaiveDate` | `Date` |
//! | `chrono::Utc::now()`, `Utc::now()` | `Timestamp::now()` |
//! | `DateTime::from_timestamp(s, 0)` (`Option`) | `Timestamp::from_unix_seconds(s)` (`Option`) |
//! | `DateTime::from_timestamp_millis(ms)` | `Timestamp::from_unix_millis(ms)` |
//! | `DateTime::from_timestamp_micros(us)` | `Timestamp::from_unix_micros(us)` |
//! | `Utc.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap()`, `TimeZone::with_ymd_and_hms(&Utc, …)` | `Timestamp::from_ymd_hms(y, mo, d, h, mi, s).unwrap()` |
//! | `DateTime::<Utc>::UNIX_EPOCH` | `Timestamp::UNIX_EPOCH` (kept) |
//! | `t.timestamp()`, `t.timestamp_millis()`, `t.timestamp_micros()` | the same (kept) |
//! | `t.timestamp_nanos_opt().unwrap_or_default()` | `t.unix_nanos()` (`i128`, exact in the whole range; chrono's was `None`, so `0`, outside 1677–2262) |
//! | `DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc)` / `.to_utc()` | `Timestamp::parse_rfc3339(s)?` |
//! | `t.to_rfc3339_opts(SecondsFormat::AutoSi, true)` | `t.to_rfc3339_auto()` |
//! | `t.to_rfc3339_opts(SecondsFormat::Millis, true)` | `t.to_rfc3339_millis()` |
//! | `t.to_rfc3339_opts(SecondsFormat::Micros, true)` | `t.to_rfc3339_micros()` |
//! | `t.to_rfc3339()` (`+00:00`) | `t.to_rfc3339_offset()` |
//! | `t.format("%Y%m%dT%H%M%SZ")` | `t.to_iso8601_basic()` |
//! | `t.trunc_subsecs(6)`, `SubsecRound::trunc_subsecs(t, 6)` | `t.trunc_micros()` |
//! | `t.date_naive()` | `t.utc_date()` |
//! | `a - b` (`TimeDelta`) | `a.since(b)` (`SignedDuration`) |
//! | `(a - b).num_seconds()` / `num_milliseconds()` / `num_minutes()` / `num_hours()` | `a.since(b).as_secs()` / `.as_millis()` (`i128`) / `.as_mins()` / `.as_hours()` |
//! | `(a - b).abs()` | `a.since(b).abs()` |
//! | `a - b >= TimeDelta::hours(24)` | `a.since(b) >= SignedDuration::from_hours(24)` (a `const`) |
//! | `chrono::Duration` / `TimeDelta` as a span to add | `std::time::Duration` |
//! | `Duration::days(n)` / `hours(n)` / `minutes(n)` / `seconds(n)` (also `TimeDelta::…`, `const`) | `days(n)` / `hours(n)` / `minutes(n)` / `seconds(n)` (`u64`, `const fn`) |
//! | `Duration::milliseconds(n)` / `microseconds(n)` | `std::time::Duration::from_millis(n)` / `from_micros(n)` |
//! | `t + Duration::hours(-1)` (negative span) | `t - hours(1)` |
//! | `t + Duration::from_std(d).unwrap()` | `t + d` |
//! | `t.checked_add_signed(Duration::from_std(d).ok()?)` | `t.checked_add(d)` |
//! | `t - Duration::from_std(d).unwrap_or(Duration::MAX)` | `t.checked_sub(d)` (`None` where chrono saturated) |
//! | `NaiveDate::from_ymd_opt(y, m, d)` | `Date::ymd(y, m, d)` (`Option`) |
//! | `NaiveDate::parse_from_str(s, "%Y-%m-%d")` | `Date::parse_iso(s)` |
//! | `chrono::ParseError` | [`ParseError`] |
//! | `humantime::parse_duration(s)` | `parse_duration(s)` |
//! | `humantime::DurationError` | [`DurationError`] (same `Display`) |
//! | `format!("{t}")`, `t.to_string()` | the same (kept, see above); prefer a `to_rfc3339_*` |
//! | `chrono-tz` `Tz`, `croner` with chrono | not here: schedules use croner's jiff backend and jiff's bundled time zones |

mod duration;
mod jiff_backed;

use std::fmt;
use std::time::Duration;

pub use duration::{DurationError, parse_duration};
/// The signed difference of two timestamps ([`TimestampExt::since`]): seconds and nanoseconds,
/// with `as_secs`, `as_millis`, `abs`, `unsigned_abs`, `is_negative` and `const`
/// constructors (`from_hours`, `from_secs`, …).
pub use jiff::SignedDuration;

pub use jiff_backed::{Date, Timestamp};

/// The traits and types most code needs.
pub mod prelude {
    pub use crate::{Date, DateExt, SignedDuration, Timestamp, TimestampExt};
}

mod sealed {
    pub trait Sealed {}
}

/// Why a timestamp or date string didn't parse. Its `Display` is chrono's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    message: &'static str,
    beyond_range: bool,
}

impl ParseError {
    pub(crate) fn new(message: &'static str, beyond_range: bool) -> Self {
        ParseError {
            message,
            beyond_range,
        }
    }

    /// True when the string was a well-formed time or date that lies past the types' range
    /// (`9999-12-31T00:00:00Z`, `+10000-01-01`), false when it was malformed. Both show as
    /// chrono's "input is out of range" or another chrono message; this tells them apart.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// assert!(Timestamp::parse_rfc3339("9999-12-31T00:00:00Z").unwrap_err().is_beyond_range());
    /// assert!(!Timestamp::parse_rfc3339("2026-02-30T00:00:00Z").unwrap_err().is_beyond_range());
    /// ```
    pub fn is_beyond_range(&self) -> bool {
        self.beyond_range
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for ParseError {}

/// `n` days. `const`, so it can define a constant.
///
/// ```
/// const RETENTION: std::time::Duration = chelabase_time::days(7);
/// assert_eq!(RETENTION.as_secs(), 604_800);
/// ```
pub const fn days(n: u64) -> Duration {
    Duration::from_secs(n * 86_400)
}

/// `n` hours.
///
/// ```
/// assert_eq!(chelabase_time::hours(2).as_secs(), 7200);
/// ```
pub const fn hours(n: u64) -> Duration {
    Duration::from_secs(n * 3600)
}

/// `n` minutes.
///
/// ```
/// assert_eq!(chelabase_time::minutes(2).as_secs(), 120);
/// ```
pub const fn minutes(n: u64) -> Duration {
    Duration::from_secs(n * 60)
}

/// `n` seconds.
///
/// ```
/// assert_eq!(chelabase_time::seconds(2).as_secs(), 2);
/// ```
pub const fn seconds(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// Building, formatting and computing with a [`Timestamp`].
pub trait TimestampExt: sealed::Sealed + Sized {
    /// The date type that goes with this timestamp type ([`Date`] for [`Timestamp`]).
    type Date: DateExt;

    /// The current time.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// assert!(Timestamp::now() > Timestamp::UNIX_EPOCH);
    /// ```
    fn now() -> Self;

    /// `secs` seconds after the Unix epoch; `None` out of range.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_seconds(1_790_769_600).unwrap();
    /// assert_eq!(t.to_rfc3339_auto(), "2026-09-30T12:00:00Z");
    /// ```
    fn from_unix_seconds(secs: i64) -> Option<Self>;

    /// `millis` milliseconds after the Unix epoch; `None` out of range.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_millis(1_790_769_600_500).unwrap();
    /// assert_eq!(t.to_rfc3339_auto(), "2026-09-30T12:00:00.500Z");
    /// ```
    fn from_unix_millis(millis: i64) -> Option<Self>;

    /// `micros` microseconds after the Unix epoch; `None` out of range.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_micros(1_900_000_000_123_456).unwrap();
    /// assert_eq!(t.timestamp_micros(), 1_900_000_000_123_456);
    /// ```
    fn from_unix_micros(micros: i64) -> Option<Self>;

    /// The UTC time `year-month-day hour:minute:second`; `None` if it doesn't exist.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_ymd_hms(2099, 1, 2, 3, 4, 5).unwrap();
    /// assert_eq!(t.to_rfc3339_auto(), "2099-01-02T03:04:05Z");
    /// ```
    fn from_ymd_hms(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self>;

    /// An RFC 3339 timestamp with `Z` or an offset, as UTC. A string without an offset is an
    /// error.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::parse_rfc3339("2026-09-30T14:00:00+02:00").unwrap();
    /// assert_eq!(t.to_rfc3339_auto(), "2026-09-30T12:00:00Z");
    /// assert!(Timestamp::parse_rfc3339("2026-09-30T12:00:00").is_err());
    /// ```
    fn parse_rfc3339(s: &str) -> Result<Self, ParseError>;

    /// RFC 3339 with `Z` and 0, 3, 6 or 9 fraction digits, as few as hold the value
    /// (`…12:00:00Z`, `…12:00:00.500Z`, `…12:00:00.123400Z`). The serde form.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_millis(1_790_769_600_500).unwrap();
    /// assert_eq!(t.to_rfc3339_auto(), "2026-09-30T12:00:00.500Z");
    /// ```
    fn to_rfc3339_auto(&self) -> String;

    /// RFC 3339 with `Z` and exactly 3 fraction digits (truncated).
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_micros(1_790_769_600_123_999).unwrap();
    /// assert_eq!(t.to_rfc3339_millis(), "2026-09-30T12:00:00.123Z");
    /// ```
    fn to_rfc3339_millis(&self) -> String;

    /// RFC 3339 with `Z` and exactly 6 fraction digits (truncated): what Postgres keeps.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_seconds(1_790_769_600).unwrap();
    /// assert_eq!(t.to_rfc3339_micros(), "2026-09-30T12:00:00.000000Z");
    /// ```
    fn to_rfc3339_micros(&self) -> String;

    /// RFC 3339 with `+00:00` and 0, 3, 6 or 9 fraction digits.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_seconds(1_790_769_600).unwrap();
    /// assert_eq!(t.to_rfc3339_offset(), "2026-09-30T12:00:00+00:00");
    /// ```
    fn to_rfc3339_offset(&self) -> String;

    /// ISO 8601 basic format to the whole second, for names: `20260930T120000Z`.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_millis(1_790_769_600_999).unwrap();
    /// assert_eq!(t.to_iso8601_basic(), "20260930T120000Z");
    /// ```
    fn to_iso8601_basic(&self) -> String;

    /// Nanoseconds since the Unix epoch, exact over the whole range.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::from_unix_micros(-1).unwrap();
    /// assert_eq!(t.unix_nanos(), -1_000);
    /// ```
    fn unix_nanos(&self) -> i128;

    /// The same instant without its sub-microsecond digits: dropped toward the past, never
    /// rounded (what a Postgres `timestamptz` keeps).
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt};
    /// let t = Timestamp::parse_rfc3339("2026-09-30T12:00:00.123456789Z").unwrap();
    /// assert_eq!(t.trunc_micros().to_rfc3339_auto(), "2026-09-30T12:00:00.123456Z");
    /// ```
    fn trunc_micros(self) -> Self;

    /// `self - earlier`, negative when `earlier` is later.
    ///
    /// ```
    /// use chelabase_time::{SignedDuration, Timestamp, TimestampExt};
    /// let a = Timestamp::from_unix_seconds(100).unwrap();
    /// let b = Timestamp::from_unix_seconds(160).unwrap();
    /// assert_eq!(b.since(a), SignedDuration::from_secs(60));
    /// assert_eq!(a.since(b), SignedDuration::from_secs(-60));
    /// ```
    fn since(&self, earlier: Self) -> SignedDuration;

    /// `self + d`; `None` past the supported range.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt, hours};
    /// let t = Timestamp::from_unix_seconds(0).unwrap();
    /// assert_eq!(t.checked_add(hours(1)).unwrap().timestamp(), 3600);
    /// assert!(t.checked_add(std::time::Duration::MAX).is_none());
    /// ```
    fn checked_add(self, d: Duration) -> Option<Self>;

    /// `self - d`; `None` past the supported range.
    ///
    /// ```
    /// use chelabase_time::{Timestamp, TimestampExt, hours};
    /// let t = Timestamp::from_unix_seconds(0).unwrap();
    /// assert_eq!(t.checked_sub(hours(1)).unwrap().timestamp(), -3600);
    /// assert!(t.checked_sub(std::time::Duration::MAX).is_none());
    /// ```
    fn checked_sub(self, d: Duration) -> Option<Self>;

    /// The calendar date in UTC.
    ///
    /// ```
    /// use chelabase_time::{Date, DateExt, Timestamp, TimestampExt};
    /// let t = Timestamp::from_ymd_hms(2026, 9, 30, 23, 59, 59).unwrap();
    /// assert_eq!(t.utc_date(), Date::ymd(2026, 9, 30).unwrap());
    /// ```
    fn utc_date(&self) -> Self::Date;
}

/// Building and parsing a [`Date`].
pub trait DateExt: sealed::Sealed + Sized {
    /// The date `year-month-day`; `None` if it doesn't exist.
    ///
    /// ```
    /// use chelabase_time::{Date, DateExt};
    /// assert_eq!(Date::ymd(2026, 10, 1).unwrap().to_string(), "2026-10-01");
    /// assert!(Date::ymd(2026, 2, 29).is_none());
    /// ```
    fn ymd(year: i32, month: u32, day: u32) -> Option<Self>;

    /// `YYYY-MM-DD` (today's `%Y-%m-%d` parse: it also takes one-digit months and days).
    ///
    /// ```
    /// use chelabase_time::{Date, DateExt};
    /// assert_eq!(Date::parse_iso("2026-10-01").unwrap(), Date::ymd(2026, 10, 1).unwrap());
    /// assert!(Date::parse_iso("2026-10-01T00:00:00Z").is_err());
    /// ```
    fn parse_iso(s: &str) -> Result<Self, ParseError>;
}
