//! Human-readable durations (`1h 30m`, `7days`, `604800s`) with humantime 2.4.0's grammar,
//! unit values and error messages, so every string an env var or the pageserver sent before
//! still parses to the same `Duration` and a bad one is reported the same way.
//!
//! Grammar: one or more `<number><unit>` items, optionally separated by whitespace. A number
//! is decimal digits (whitespace between digits is skipped) with an optional fraction; units
//! are case-sensitive. The bare string `0` is zero. Units: `nanos nsec ns`, `usec us µs`,
//! `millis msec ms`, `seconds second secs sec s`, `minutes minute min mins m`,
//! `hours hour hr hrs h`, `days day d`, `weeks week wk wks w`, `months month M` (30.44 days),
//! `years year yr yrs y` (365.25 days).

use std::fmt;
use std::str::Chars;
use std::time::Duration;

/// Why [`parse_duration`] refused a string. Its `Display` is humantime 2.4.0's, word for word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurationError {
    /// A character that can't appear there, at this byte offset.
    InvalidCharacter(usize),
    /// A number was expected at this byte offset.
    NumberExpected(usize),
    /// The unit after `value` is unknown or missing (empty `unit`).
    UnknownUnit {
        start: usize,
        end: usize,
        unit: String,
        value: u64,
    },
    /// The value doesn't fit a `Duration`, or has a fraction below a nanosecond.
    NumberOverflow,
    /// Nothing but whitespace.
    Empty,
}

impl std::error::Error for DurationError {}

impl fmt::Display for DurationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCharacter(offset) => write!(f, "invalid character at {offset}"),
            Self::NumberExpected(offset) => write!(f, "expected number at {offset}"),
            Self::UnknownUnit { unit, value, .. } if unit.is_empty() => {
                write!(f, "time unit needed, for example {value}sec or {value}ms")
            }
            Self::UnknownUnit { unit, .. } => write!(
                f,
                "unknown time unit {unit:?}, supported units: ns, us/µs, ms, sec, min, hours, \
                 days, weeks, months, years (and few variations)"
            ),
            Self::NumberOverflow => f.write_str(
                "number is too large or cannot be represented without a lack of precision \
                 (values below 1ns are not supported)",
            ),
            Self::Empty => f.write_str("value was empty"),
        }
    }
}

/// Parses a human-readable duration such as `1h 30m` or `7days` (see the module docs).
///
/// ```
/// use std::time::Duration;
/// use chelabase_time::parse_duration;
///
/// assert_eq!(parse_duration("1h 30m").unwrap(), Duration::from_secs(5400));
/// assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
/// assert!(parse_duration("5").is_err());
/// ```
pub fn parse_duration(s: &str) -> Result<Duration, DurationError> {
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    Parser {
        iter: s.chars(),
        src: s,
    }
    .parse()
}

const NANOS_PER_SEC: u64 = 1_000_000_000;

fn mul(a: u64, b: u64) -> Result<u64, DurationError> {
    a.checked_mul(b).ok_or(DurationError::NumberOverflow)
}

fn add(a: u64, b: u64) -> Result<u64, DurationError> {
    a.checked_add(b).ok_or(DurationError::NumberOverflow)
}

/// Exact division only: a remainder would lose precision.
fn div(a: u64, b: u64) -> Result<u64, DurationError> {
    if a.is_multiple_of(b) {
        Ok(a / b)
    } else {
        Err(DurationError::NumberOverflow)
    }
}

#[derive(Clone, Copy)]
struct Fraction {
    numerator: u64,
    denominator: u64,
}

#[derive(Clone, Copy)]
enum Unit {
    Nanosecond,
    Microsecond,
    Millisecond,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Month,
    Year,
}

impl Unit {
    fn from_name(s: &str) -> Option<Self> {
        Some(match s {
            "nanos" | "nsec" | "ns" => Self::Nanosecond,
            "usec" | "us" | "µs" => Self::Microsecond,
            "millis" | "msec" | "ms" => Self::Millisecond,
            "seconds" | "second" | "secs" | "sec" | "s" => Self::Second,
            "minutes" | "minute" | "min" | "mins" | "m" => Self::Minute,
            "hours" | "hour" | "hr" | "hrs" | "h" => Self::Hour,
            "days" | "day" | "d" => Self::Day,
            "weeks" | "week" | "wk" | "wks" | "w" => Self::Week,
            "months" | "month" | "M" => Self::Month,
            "years" | "year" | "yr" | "yrs" | "y" => Self::Year,
            _ => return None,
        })
    }

    /// Seconds in a whole unit (zero below a second).
    fn secs(self) -> u64 {
        match self {
            Self::Nanosecond | Self::Microsecond | Self::Millisecond => 0,
            Self::Second => 1,
            Self::Minute => 60,
            Self::Hour => 3600,
            Self::Day => 86_400,
            Self::Week => 7 * 86_400,
            Self::Month => 2_630_016, // 30.44 days
            Self::Year => 31_557_600, // 365.25 days
        }
    }
}

fn is_unit_char(c: char) -> bool {
    c.is_ascii_alphabetic() || c == 'µ'
}

struct Parser<'a> {
    iter: Chars<'a>,
    src: &'a str,
}

impl Parser<'_> {
    fn off(&self) -> usize {
        self.src.len() - self.iter.as_str().len()
    }

    fn parse(mut self) -> Result<Duration, DurationError> {
        let mut n = self.first_digit()?.ok_or(DurationError::Empty)?;
        let mut out = Duration::ZERO;
        'item: loop {
            // The number: more digits (whitespace between them skipped), then a fraction or
            // the unit's first letter.
            let mut frac = None;
            let mut off = self.off();
            while let Some(c) = self.iter.next() {
                match c {
                    '0'..='9' => {
                        n = n
                            .checked_mul(10)
                            .and_then(|x| x.checked_add(u64::from(c) - u64::from('0')))
                            .ok_or(DurationError::NumberOverflow)?;
                    }
                    c if c.is_whitespace() => {}
                    c if is_unit_char(c) => break,
                    '.' => {
                        frac = Some(self.fraction(&mut off)?);
                        break;
                    }
                    _ => return Err(DurationError::InvalidCharacter(off)),
                }
                off = self.off();
            }
            // The unit: letters up to whitespace, the next number or the end.
            let start = off;
            let mut off = self.off();
            while let Some(c) = self.iter.next() {
                match c {
                    '0'..='9' => {
                        self.add_unit(n, frac, start, off, &mut out)?;
                        n = u64::from(c) - u64::from('0');
                        continue 'item;
                    }
                    c if c.is_whitespace() => break,
                    c if is_unit_char(c) => {}
                    _ => return Err(DurationError::InvalidCharacter(off)),
                }
                off = self.off();
            }
            self.add_unit(n, frac, start, off, &mut out)?;
            n = match self.first_digit()? {
                Some(n) => n,
                None => return Ok(out),
            };
        }
    }

    /// The first digit of the next item, past whitespace; `None` at the end.
    fn first_digit(&mut self) -> Result<Option<u64>, DurationError> {
        let off = self.off();
        for c in self.iter.by_ref() {
            match c {
                '0'..='9' => return Ok(Some(u64::from(c) - u64::from('0'))),
                c if c.is_whitespace() => {}
                _ => return Err(DurationError::NumberExpected(off)),
            }
        }
        Ok(None)
    }

    /// The digits after a `.`, up to the unit's first letter.
    fn fraction(&mut self, off: &mut usize) -> Result<Fraction, DurationError> {
        let mut numerator = 0u64;
        let mut denominator = 1u64;
        let mut zeros = true;
        while let Some(c) = self.iter.next() {
            match c {
                '0' => {
                    denominator = mul(denominator, 10)?;
                    if !zeros {
                        numerator = mul(numerator, 10)?;
                    }
                }
                '1'..='9' => {
                    zeros = false;
                    denominator = mul(denominator, 10)?;
                    numerator = add(mul(numerator, 10)?, u64::from(c) - u64::from('0'))?;
                }
                c if c.is_whitespace() => {}
                c if is_unit_char(c) => break,
                _ => return Err(DurationError::InvalidCharacter(*off)),
            }
            *off = self.off();
        }
        if denominator == 1 {
            return Err(DurationError::InvalidCharacter(*off));
        }
        Ok(Fraction {
            numerator,
            denominator,
        })
    }

    fn add_unit(
        &self,
        n: u64,
        frac: Option<Fraction>,
        start: usize,
        end: usize,
        out: &mut Duration,
    ) -> Result<(), DurationError> {
        let name = &self.src[start..end];
        let Some(unit) = Unit::from_name(name) else {
            return Err(DurationError::UnknownUnit {
                start,
                end,
                unit: name.to_owned(),
                value: n,
            });
        };
        let (sec, nsec) = match unit {
            Unit::Nanosecond => (0, n),
            Unit::Microsecond => (0, mul(n, 1000)?),
            Unit::Millisecond => (0, mul(n, 1_000_000)?),
            _ => (mul(n, unit.secs())?, 0),
        };
        add_to(sec, nsec, out)?;
        if let Some(Fraction {
            numerator: n,
            denominator: d,
        }) = frac
        {
            let (sec, nsec) = match unit {
                Unit::Nanosecond => return Err(DurationError::NumberOverflow),
                Unit::Microsecond => (0, div(mul(n, 1000)?, d)?),
                Unit::Millisecond => (0, div(mul(n, 1_000_000)?, d)?),
                Unit::Second => (0, div(mul(n, NANOS_PER_SEC)?, d)?),
                Unit::Minute => (0, div(mul(n, 60 * NANOS_PER_SEC)?, d)?),
                _ => (div(mul(n, unit.secs())?, d)?, 0),
            };
            add_to(sec, nsec, out)?;
        }
        Ok(())
    }
}

/// Adds `sec` seconds and `nsec` nanoseconds (which may exceed a second) to `out`.
/// humantime carries only above 1e9 and lets `Duration::new` carry exactly 1e9, which panics at
/// `u64::MAX` seconds; this carries both and returns the overflow error instead.
fn add_to(sec: u64, nsec: u64, out: &mut Duration) -> Result<(), DurationError> {
    let nsec = add(u64::from(out.subsec_nanos()), nsec)?;
    let sec = add(add(sec, nsec / NANOS_PER_SEC)?, out.as_secs())?;
    // Below 1e9 after the carry, so the cast and `Duration::new` can't fail.
    *out = Duration::new(sec, (nsec % NANOS_PER_SEC) as u32);
    Ok(())
}
