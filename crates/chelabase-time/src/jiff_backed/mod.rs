//! The jiff-backed [`Timestamp`] and [`Date`] that replace the chrono aliases when chrono leaves
//! the tree. They keep every contract the crate docs list, with the exceptions in the crate
//! docs' "Differences" section, and are proven equal to the chrono backing by
//! `tests/parity.rs` while chrono is still here to compare against.

mod parse;
#[cfg(feature = "sqlx")]
mod pg;
#[cfg(feature = "sqlx")]
pub use pg::{decode_pg_binary_micros, decode_pg_date_text, decode_pg_text};

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use jiff::tz::Offset;

use crate::{DateExt, ParseError, SignedDuration, TimestampExt};

const NANOS_PER_SEC: i128 = 1_000_000_000;

/// An instant in UTC, nanosecond precision, backed by [`jiff::Timestamp`].
///
/// Its range is jiff's: about `-9999-01-02T01:59:59Z` to `9999-12-30T22:00:00Z`. Every
/// constructor and parser returns `None` or an error outside it, never a clamped value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(jiff::Timestamp);

/// A calendar date without a time zone, backed by [`jiff::civil::Date`] (years -9999..=9999).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(jiff::civil::Date);

impl Timestamp {
    /// `1970-01-01T00:00:00Z`.
    pub const UNIX_EPOCH: Timestamp = Timestamp(jiff::Timestamp::UNIX_EPOCH);

    /// Whole seconds since the Unix epoch, rounded toward the past.
    pub fn timestamp(&self) -> i64 {
        self.floor_div(NANOS_PER_SEC)
    }

    /// Whole milliseconds since the Unix epoch, rounded toward the past.
    pub fn timestamp_millis(&self) -> i64 {
        self.floor_div(1_000_000)
    }

    /// Whole microseconds since the Unix epoch, rounded toward the past
    /// (`1969-12-31T23:59:59.999999999Z` is `-1`).
    pub fn timestamp_micros(&self) -> i64 {
        self.floor_div(1_000)
    }

    /// The jiff value underneath.
    pub fn to_jiff(self) -> jiff::Timestamp {
        self.0
    }

    /// Wraps a jiff value.
    pub fn from_jiff(t: jiff::Timestamp) -> Self {
        Timestamp(t)
    }

    fn floor_div(&self, unit: i128) -> i64 {
        // jiff's range in nanoseconds divided by at least 1000 fits an i64.
        self.0.as_nanosecond().div_euclid(unit) as i64
    }

    fn from_nanos(nanos: i128) -> Option<Self> {
        jiff::Timestamp::from_nanosecond(nanos).ok().map(Timestamp)
    }

    fn parse_with(
        s: &str,
        parser: fn(&str) -> Result<i128, parse::Kind>,
    ) -> Result<Self, ParseError> {
        let nanos = parser(s).map_err(kind_error)?;
        Self::from_nanos(nanos).ok_or_else(|| kind_error(parse::Kind::OutOfRange))
    }

    /// Writes the UTC date and time as chrono's `write_rfc3339` does, up to the offset.
    fn write_rfc3339(&self, f: &mut impl fmt::Write, fraction: Fraction, sep: char) -> fmt::Result {
        let dt = Offset::UTC.to_datetime(self.0);
        write_year(f, dt.year())?;
        write!(
            f,
            "-{:02}-{:02}{sep}{:02}:{:02}:{:02}",
            dt.month(),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second()
        )?;
        // A civil time's fraction is never negative.
        let nano = dt.subsec_nanosecond() as u32;
        match fraction {
            Fraction::Millis => write!(f, ".{:03}", nano / 1_000_000),
            Fraction::Micros => write!(f, ".{:06}", nano / 1_000),
            Fraction::Auto => write_auto_fraction(f, nano),
        }
    }

    fn rfc3339_string(&self, fraction: Fraction, zone: &str) -> String {
        let mut s = String::with_capacity(36);
        // Writing to a String can't fail.
        let _ = self.write_rfc3339(&mut s, fraction, 'T');
        s.push_str(zone);
        s
    }
}

#[derive(Clone, Copy)]
enum Fraction {
    Auto,
    Millis,
    Micros,
}

/// chrono's year: four digits in 0..=9999, otherwise signed with at least four digits.
fn write_year(f: &mut impl fmt::Write, year: i16) -> fmt::Result {
    if (0..=9999).contains(&year) {
        write!(f, "{year:04}")
    } else {
        write!(f, "{year:+05}")
    }
}

/// chrono's `SecondsFormat::AutoSi` and `NaiveTime`'s `Display`: none, 3, 6 or 9 digits.
fn write_auto_fraction(f: &mut impl fmt::Write, nano: u32) -> fmt::Result {
    if nano == 0 {
        Ok(())
    } else if nano.is_multiple_of(1_000_000) {
        write!(f, ".{:03}", nano / 1_000_000)
    } else if nano.is_multiple_of(1_000) {
        write!(f, ".{:06}", nano / 1_000)
    } else {
        write!(f, ".{nano:09}")
    }
}

fn kind_error(kind: parse::Kind) -> ParseError {
    ParseError(kind.message().to_owned())
}

impl crate::sealed::Sealed for Timestamp {}
impl crate::sealed::Sealed for Date {}

impl TimestampExt for Timestamp {
    type Date = Date;

    fn now() -> Self {
        Timestamp(jiff::Timestamp::now())
    }

    fn from_unix_seconds(secs: i64) -> Option<Self> {
        jiff::Timestamp::from_second(secs).ok().map(Timestamp)
    }

    fn from_unix_millis(millis: i64) -> Option<Self> {
        jiff::Timestamp::from_millisecond(millis)
            .ok()
            .map(Timestamp)
    }

    fn from_unix_micros(micros: i64) -> Option<Self> {
        jiff::Timestamp::from_microsecond(micros)
            .ok()
            .map(Timestamp)
    }

    fn from_ymd_hms(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> Option<Self> {
        let dt = jiff::civil::DateTime::new(
            i16::try_from(year).ok()?,
            i8::try_from(month).ok()?,
            i8::try_from(day).ok()?,
            i8::try_from(hour).ok()?,
            i8::try_from(minute).ok()?,
            i8::try_from(second).ok()?,
            0,
        )
        .ok()?;
        Offset::UTC.to_timestamp(dt).ok().map(Timestamp)
    }

    fn parse_rfc3339(s: &str) -> Result<Self, ParseError> {
        Self::parse_with(s, parse::rfc3339_strict)
    }

    fn to_rfc3339_auto(&self) -> String {
        self.rfc3339_string(Fraction::Auto, "Z")
    }

    fn to_rfc3339_millis(&self) -> String {
        self.rfc3339_string(Fraction::Millis, "Z")
    }

    fn to_rfc3339_micros(&self) -> String {
        self.rfc3339_string(Fraction::Micros, "Z")
    }

    fn to_rfc3339_offset(&self) -> String {
        self.rfc3339_string(Fraction::Auto, "+00:00")
    }

    fn to_iso8601_basic(&self) -> String {
        let dt = Offset::UTC.to_datetime(self.0);
        let mut s = String::with_capacity(16);
        let _ = write_year(&mut s, dt.year());
        let _ = fmt::Write::write_fmt(
            &mut s,
            format_args!(
                "{:02}{:02}T{:02}{:02}{:02}Z",
                dt.month(),
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second()
            ),
        );
        s
    }

    fn unix_nanos(&self) -> i128 {
        self.0.as_nanosecond()
    }

    fn trunc_micros(self) -> Self {
        let nanos = self.0.as_nanosecond();
        // Truncating toward the past stays above jiff's minimum, a whole second.
        Self::from_nanos(nanos - nanos.rem_euclid(1_000)).unwrap_or(self)
    }

    fn since(&self, earlier: Self) -> SignedDuration {
        self.0.duration_since(earlier.0)
    }

    fn checked_add(self, d: Duration) -> Option<Self> {
        let d = SignedDuration::try_from(d).ok()?;
        self.0.checked_add(d).ok().map(Timestamp)
    }

    fn checked_sub(self, d: Duration) -> Option<Self> {
        let d = SignedDuration::try_from(d).ok()?;
        self.0.checked_sub(d).ok().map(Timestamp)
    }

    fn utc_date(&self) -> Date {
        Date(Offset::UTC.to_datetime(self.0).date())
    }
}

impl std::ops::Add<Duration> for Timestamp {
    type Output = Timestamp;

    /// Panics past the supported range; [`TimestampExt::checked_add`] doesn't.
    fn add(self, d: Duration) -> Timestamp {
        self.checked_add(d)
            .expect("`Timestamp + Duration` overflowed")
    }
}

impl std::ops::Sub<Duration> for Timestamp {
    type Output = Timestamp;

    /// Panics past the supported range; [`TimestampExt::checked_sub`] doesn't.
    fn sub(self, d: Duration) -> Timestamp {
        self.checked_sub(d)
            .expect("`Timestamp - Duration` overflowed")
    }
}

impl std::ops::AddAssign<Duration> for Timestamp {
    fn add_assign(&mut self, d: Duration) {
        *self = *self + d;
    }
}

impl std::ops::SubAssign<Duration> for Timestamp {
    fn sub_assign(&mut self, d: Duration) {
        *self = *self - d;
    }
}

/// `a - b` is [`TimestampExt::since`]: signed, negative when `b` is later. It can't overflow.
impl std::ops::Sub<Timestamp> for Timestamp {
    type Output = SignedDuration;

    fn sub(self, earlier: Timestamp) -> SignedDuration {
        self.since(earlier)
    }
}

/// `2026-09-30 12:00:00.500 UTC`: chrono's `Display` for `DateTime<Utc>`.
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_rfc3339(f, Fraction::Auto, ' ')?;
        f.write_str(" UTC")
    }
}

/// `2026-09-30T12:00:00.500Z` (chrono's `Debug` for `DateTime<Utc>`; not a contract).
impl fmt::Debug for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_rfc3339(f, Fraction::Auto, 'T')?;
        f.write_str("Z")
    }
}

/// chrono's `DateTime<Utc>` `FromStr`: relaxed RFC 3339, see the crate docs.
impl FromStr for Timestamp {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        Self::parse_with(s, parse::rfc3339_relaxed)
    }
}

impl serde::Serialize for Timestamp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_rfc3339_auto())
    }
}

impl<'de> serde::Deserialize<'de> for Timestamp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Timestamp;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an RFC 3339 formatted date and time string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Timestamp, E> {
                value.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}

impl Date {
    /// The jiff value underneath.
    pub fn to_jiff(self) -> jiff::civil::Date {
        self.0
    }

    /// Wraps a jiff value.
    pub fn from_jiff(d: jiff::civil::Date) -> Self {
        Date(d)
    }

    fn from_ymd(ymd: parse::Ymd) -> Result<Self, ParseError> {
        let year = i16::try_from(ymd.year).map_err(|_| kind_error(parse::Kind::OutOfRange))?;
        // `Ymd` holds a valid month and day; only the year can be past jiff's range.
        jiff::civil::Date::new(year, ymd.month as i8, ymd.day as i8)
            .map(Date)
            .map_err(|_| kind_error(parse::Kind::OutOfRange))
    }
}

impl DateExt for Date {
    fn ymd(year: i32, month: u32, day: u32) -> Option<Self> {
        jiff::civil::Date::new(
            i16::try_from(year).ok()?,
            i8::try_from(month).ok()?,
            i8::try_from(day).ok()?,
        )
        .ok()
        .map(Date)
    }

    fn parse_iso(s: &str) -> Result<Self, ParseError> {
        Self::from_ymd(parse::date_iso(s).map_err(kind_error)?)
    }
}

/// `2026-10-01`: chrono's `NaiveDate` `Display` (signed years outside 0..=9999).
impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_year(f, self.0.year())?;
        write!(f, "-{:02}-{:02}", self.0.month(), self.0.day())
    }
}

/// The same as `Display`, as chrono's.
impl fmt::Debug for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// chrono's `NaiveDate` `FromStr`: `%Y-%m-%d` with spaces allowed around the dashes and after.
impl FromStr for Date {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        Self::from_ymd(parse::date_relaxed(s).map_err(kind_error)?)
    }
}

impl serde::Serialize for Date {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Date {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = Date;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a formatted date string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Date, E> {
                value.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_str(Visitor)
    }
}
