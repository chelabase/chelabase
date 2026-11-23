//! The parsers chrono 0.4.45 runs for `Timestamp` and `Date`, ported so the jiff backing accepts,
//! rejects and reports the same strings with the same messages:
//!
//! - [`rfc3339_strict`]: `DateTime::parse_from_rfc3339` (`TimestampExt::parse_rfc3339`);
//! - [`rfc3339_relaxed`]: `DateTime<Utc>`'s `FromStr`, which its serde `Deserialize` calls;
//! - [`date_iso`]: `NaiveDate::parse_from_str(s, "%Y-%m-%d")` (`DateExt::parse_iso`);
//! - [`date_relaxed`]: `NaiveDate`'s `FromStr`, which its serde `Deserialize` calls.
//!
//! Each returns the instant as nanoseconds since the Unix epoch, or the date as fields, and leaves
//! the range check to the caller (jiff's range is smaller than chrono's; see the crate docs).

/// chrono's `ParseErrorKind`s that these parsers can return; `message` is chrono's `Display`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    OutOfRange,
    Invalid,
    TooShort,
    TooLong,
}

impl Kind {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Kind::OutOfRange => "input is out of range",
            Kind::Invalid => "input contains invalid characters",
            Kind::TooShort => "premature end of input",
            Kind::TooLong => "trailing input",
        }
    }
}

type Scan<'a, T> = Result<(&'a str, T), Kind>;

const NANOS_PER_SEC: i128 = 1_000_000_000;

/// chrono's `scan::number`: `min` to `max` ASCII digits; digits past `max` are left in `s`.
fn number(s: &str, min: usize, max: usize) -> Scan<'_, i64> {
    let bytes = s.as_bytes();
    if bytes.len() < min {
        return Err(Kind::TooShort);
    }
    let mut n = 0i64;
    for (i, c) in bytes.iter().take(max).copied().enumerate() {
        if !c.is_ascii_digit() {
            if i < min {
                return Err(Kind::Invalid);
            }
            return Ok((&s[i..], n));
        }
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(i64::from(c - b'0')))
            .ok_or(Kind::OutOfRange)?;
    }
    Ok((&s[max.min(bytes.len())..], n))
}

/// chrono's `scan::nanosecond`: one or more digits after the `.`; digits past nine are skipped.
fn nanosecond(s: &str) -> Scan<'_, u32> {
    let origlen = s.len();
    let (s, v) = number(s, 1, 9)?;
    let consumed = origlen - s.len();
    const SCALE: [i64; 10] = [
        0,
        100_000_000,
        10_000_000,
        1_000_000,
        100_000,
        10_000,
        1_000,
        100,
        10,
        1,
    ];
    let v = v * SCALE[consumed];
    let s = s.trim_start_matches(|c: char| c.is_ascii_digit());
    // `v` is at most 999_999_999.
    Ok((s, v as u32))
}

/// chrono's `scan::timezone_offset` with `allow_zulu`, required minutes and U+2212 allowed as
/// the minus sign. Returns the offset in seconds east of UTC.
fn timezone_offset<'a>(
    mut s: &'a str,
    consume_colon: fn(&str) -> Result<&str, Kind>,
) -> Scan<'a, i32> {
    if let Some(b'Z' | b'z') = s.as_bytes().first() {
        return Ok((&s[1..], 0));
    }
    fn digits(s: &str) -> Result<(u8, u8), Kind> {
        let b = s.as_bytes();
        if b.len() < 2 {
            Err(Kind::TooShort)
        } else {
            Ok((b[0], b[1]))
        }
    }
    let negative = match s.chars().next() {
        Some('+') => {
            s = &s[1..];
            false
        }
        Some('-') => {
            s = &s[1..];
            true
        }
        Some('−') => {
            s = &s['−'.len_utf8()..];
            true
        }
        Some(_) => return Err(Kind::Invalid),
        None => return Err(Kind::TooShort),
    };
    let hours = match digits(s)? {
        (h1 @ b'0'..=b'9', h2 @ b'0'..=b'9') => i32::from((h1 - b'0') * 10 + (h2 - b'0')),
        _ => return Err(Kind::Invalid),
    };
    s = &s[2..];
    s = consume_colon(s)?;
    let minutes = match digits(s) {
        Ok((m1 @ b'0'..=b'5', m2 @ b'0'..=b'9')) => i32::from((m1 - b'0') * 10 + (m2 - b'0')),
        Ok((b'6'..=b'9', b'0'..=b'9')) => return Err(Kind::OutOfRange),
        Ok(_) => return Err(Kind::Invalid),
        Err(_) => return Err(Kind::TooShort),
    };
    // Both bytes were ASCII digits.
    s = &s[2..];
    let seconds = hours * 3600 + minutes * 60;
    Ok((s, if negative { -seconds } else { seconds }))
}

fn colon(s: &str) -> Result<&str, Kind> {
    match s.as_bytes().first() {
        Some(b':') => Ok(&s[1..]),
        Some(_) => Err(Kind::Invalid),
        None => Err(Kind::TooShort),
    }
}

fn colon_or_space(s: &str) -> Result<&str, Kind> {
    Ok(s.trim_start_matches(|c: char| c == ':' || c.is_whitespace()))
}

/// chrono's `Item::Literal`.
fn literal<'a>(s: &'a str, prefix: &str) -> Result<&'a str, Kind> {
    if s.len() < prefix.len() {
        return Err(Kind::TooShort);
    }
    s.strip_prefix(prefix).ok_or(Kind::Invalid)
}

/// chrono's `Item::Numeric`: leading whitespace skipped, then `width` digits at most; a year
/// may carry a sign, and then any number of digits.
fn numeric(s: &str, width: usize, signed: bool) -> Scan<'_, i64> {
    let s = s.trim_start();
    if signed {
        if let Some(rest) = s.strip_prefix('-') {
            let (s, v) = number(rest, 1, usize::MAX)?;
            return Ok((s, 0i64.checked_sub(v).ok_or(Kind::OutOfRange)?));
        }
        if let Some(rest) = s.strip_prefix('+') {
            return number(rest, 1, usize::MAX);
        }
    }
    number(s, 1, width)
}

/// chrono's `NaiveDate` range: years -262143..=262142 (chrono 0.4.45 `MIN_YEAR`/`MAX_YEAR`).
const CHRONO_MIN_YEAR: i64 = -262_143;
const CHRONO_MAX_YEAR: i64 = 262_142;

fn is_leap(year: i64) -> bool {
    year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0)
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if is_leap(year) => 29,
        _ => 28,
    }
}

/// A proleptic Gregorian date that chrono's `NaiveDate::from_ymd_opt` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ymd {
    pub(crate) year: i64,
    pub(crate) month: u32,
    pub(crate) day: u32,
}

impl Ymd {
    fn new(year: i64, month: i64, day: i64) -> Result<Ymd, Kind> {
        let (Ok(month), Ok(day)) = (u32::try_from(month), u32::try_from(day)) else {
            return Err(Kind::OutOfRange);
        };
        if !(CHRONO_MIN_YEAR..=CHRONO_MAX_YEAR).contains(&year)
            || !(1..=12).contains(&month)
            || day == 0
            || day > days_in_month(year, month)
        {
            return Err(Kind::OutOfRange);
        }
        Ok(Ymd { year, month, day })
    }

    /// Days since 1970-01-01 (Howard Hinnant's `days_from_civil`).
    pub(crate) fn unix_days(self) -> i64 {
        let y = if self.month <= 2 {
            self.year - 1
        } else {
            self.year
        };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }
}

/// Date, time and offset fields, combined as chrono's `Parsed::to_datetime` does.
struct Fields {
    date: Ymd,
    hour: i64,
    minute: i64,
    /// 0..=60; 60 is a leap second, kept as chrono keeps it: second 59 plus a whole second of
    /// nanoseconds, which is the same instant as the next minute's second 0.
    second: i64,
    nano: u32,
    offset: i32,
}

impl Fields {
    fn unix_nanos(&self) -> Result<i128, Kind> {
        let (second, extra) = if self.second == 60 {
            (59, NANOS_PER_SEC)
        } else {
            (self.second, 0)
        };
        if !(0..=23).contains(&self.hour) || !(0..=59).contains(&self.minute) || second > 59 {
            return Err(Kind::OutOfRange);
        }
        if !(-86_399..=86_399).contains(&self.offset) {
            return Err(Kind::OutOfRange);
        }
        let secs = i128::from(self.date.unix_days()) * 86_400
            + i128::from(self.hour * 3600 + self.minute * 60 + second)
            - i128::from(self.offset);
        Ok(secs * NANOS_PER_SEC + extra + i128::from(self.nano))
    }
}

/// `DateTime::parse_from_rfc3339`: `YYYY-MM-DD`, `T`/`t`/space, `HH:MM:SS`, an optional
/// fraction of one or more digits, then `Z`/`z` or `±HH:MM` (up to 23:59), and nothing else.
pub(crate) fn rfc3339_strict(s: &str) -> Result<i128, Kind> {
    let bytes = s.as_bytes();
    if bytes.len() < 19 {
        return Err(Kind::TooShort);
    }
    let digit = |i: usize| -> Result<i64, Kind> {
        if bytes[i].is_ascii_digit() {
            Ok(i64::from(bytes[i] - b'0'))
        } else {
            Err(Kind::Invalid)
        }
    };
    let year = digit(0)? * 1000 + digit(1)? * 100 + digit(2)? * 10 + digit(3)?;
    if bytes[4] != b'-' {
        return Err(Kind::Invalid);
    }
    let month = digit(5)? * 10 + digit(6)?;
    if bytes[7] != b'-' {
        return Err(Kind::Invalid);
    }
    let day = digit(8)? * 10 + digit(9)?;
    let date = Ymd::new(year, month, day)?;
    if !matches!(bytes[10], b't' | b'T' | b' ') {
        return Err(Kind::Invalid);
    }
    let hour = digit(11)? * 10 + digit(12)?;
    if bytes[13] != b':' {
        return Err(Kind::Invalid);
    }
    let minute = digit(14)? * 10 + digit(15)?;
    if bytes[16] != b':' {
        return Err(Kind::Invalid);
    }
    let second = digit(17)? * 10 + digit(18)?;
    // Every byte so far is ASCII, so 19 and 20 are char boundaries.
    let (rest, nano) = if bytes.get(19) == Some(&b'.') {
        nanosecond(&s[20..])?
    } else {
        (&s[19..], 0)
    };
    // chrono checks the time (`NaiveTime::from_hms_nano_opt`) before the offset.
    if hour > 23 || minute > 59 || second > 60 {
        return Err(Kind::OutOfRange);
    }
    let (rest, offset) = timezone_offset(rest, colon)?;
    if !rest.is_empty() {
        return Err(Kind::TooLong);
    }
    Fields {
        date,
        hour,
        minute,
        second,
        nano,
        offset,
    }
    .unix_nanos()
}

fn set_range(v: i64, range: std::ops::RangeInclusive<i64>) -> Result<i64, Kind> {
    if range.contains(&v) {
        Ok(v)
    } else {
        Err(Kind::OutOfRange)
    }
}

fn year_field(v: i64) -> Result<i64, Kind> {
    i32::try_from(v)
        .map(i64::from)
        .map_err(|_| Kind::OutOfRange)
}

/// `Year`, optional space, `-`, `Month`, optional space, `-`, `Day`: chrono's relaxed date items.
/// `spaces` adds the `Item::Space("")` between them (the `FromStr` form).
fn date_items(s: &str, spaces: bool) -> Scan<'_, (i64, i64, i64)> {
    fn space(s: &str, spaces: bool) -> &str {
        if spaces { s.trim_start() } else { s }
    }
    let (s, year) = numeric(s, 4, true)?;
    let year = year_field(year)?;
    let s = literal(space(s, spaces), "-")?;
    let (s, month) = numeric(s, 2, false)?;
    let month = set_range(month, 1..=12)?;
    let s = literal(space(s, spaces), "-")?;
    let (s, day) = numeric(s, 2, false)?;
    let day = set_range(day, 1..=31)?;
    Ok((s, (year, month, day)))
}

/// `DateTime<Utc>`'s `FromStr` (and so chrono's serde): `parse_rfc3339_relaxed`, then only
/// whitespace may follow.
pub(crate) fn rfc3339_relaxed(s: &str) -> Result<i128, Kind> {
    let (s, (year, month, day)) = date_items(s, true)?;
    let s = match s.as_bytes().first() {
        Some(b't' | b'T' | b' ') => &s[1..],
        Some(_) => return Err(Kind::Invalid),
        None => return Err(Kind::TooShort),
    };
    let (s, hour) = numeric(s, 2, false)?;
    let hour = set_range(hour, 0..=23)?;
    let s = literal(s.trim_start(), ":")?;
    let (s, minute) = numeric(s, 2, false)?;
    let minute = set_range(minute, 0..=59)?;
    let s = literal(s.trim_start(), ":")?;
    let (s, second) = numeric(s, 2, false)?;
    let second = set_range(second, 0..=60)?;
    let (s, nano) = match s.strip_prefix('.') {
        Some(rest) => nanosecond(rest)?,
        None => (s, 0),
    };
    let s = s.trim_start();
    let (s, offset) = if s.len() >= 3 && s.as_bytes()[..3].eq_ignore_ascii_case(b"UTC") {
        (&s[3..], 0)
    } else {
        timezone_offset(s, colon_or_space)?
    };
    if !s.trim_start().is_empty() {
        return Err(Kind::TooLong);
    }
    // `Parsed::to_datetime`: the date, then the time, then the offset's range.
    let date = Ymd::new(year, month, day)?;
    let nanos = Fields {
        date,
        hour,
        minute,
        second,
        nano,
        offset,
    }
    .unix_nanos()?;
    Ok(nanos)
}

/// `NaiveDate::parse_from_str(s, "%Y-%m-%d")`.
pub(crate) fn date_iso(s: &str) -> Result<Ymd, Kind> {
    let (s, (year, month, day)) = date_items(s, false)?;
    if !s.is_empty() {
        return Err(Kind::TooLong);
    }
    Ymd::new(year, month, day)
}

/// `NaiveDate`'s `FromStr` (and so chrono's serde): spaces allowed around the dashes and after.
pub(crate) fn date_relaxed(s: &str) -> Result<Ymd, Kind> {
    let (s, (year, month, day)) = date_items(s, true)?;
    if !s.trim_start().is_empty() {
        return Err(Kind::TooLong);
    }
    Ymd::new(year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_days_matches_known_dates() {
        let d = |year, month, day| Ymd { year, month, day }.unix_days();
        assert_eq!(d(1970, 1, 1), 0);
        assert_eq!(d(2000, 1, 1), 10_957);
        assert_eq!(d(1969, 12, 31), -1);
        assert_eq!(d(2026, 9, 30), 20_726);
        assert_eq!(d(0, 3, 1), -719_468);
        assert_eq!(d(-1, 12, 31), -719_529);
    }
}
