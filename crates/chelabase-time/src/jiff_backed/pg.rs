//! Postgres codecs for the jiff-backed types, written to match sqlx 0.9's chrono codecs.
//!
//! Encoding goes through `jiff_sqlx`: a `timestamptz` bind is the microseconds since
//! `2000-01-01T00:00:00Z`, cut toward 2000 as chrono's `num_microseconds` cuts it, and a `date`
//! bind the days since `2000-01-01`. Decoding is ours so it can be tested without a server:
//! binary is the same count, text is what Postgres prints (`2026-09-30 17:45:00+05:45`), and
//! `infinity`/`-infinity` or a value past jiff's range is an error, never a panic.
//!
//! The type infos are the built-in OIDs (`timestamptz` 1184, `timestamptz[]` 1185, `date` 1082,
//! `date[]` 1182): sqlx keeps its named constants crate-private. They compare equal to sqlx's
//! own (`==` matches on the OID) and bind the same parameter types.

use sqlx::encode::{Encode, IsNull};
use sqlx::error::BoxDynError;
use sqlx::postgres::types::Oid;
use sqlx::postgres::{
    PgArgumentBuffer, PgHasArrayType, PgTypeInfo, PgValueFormat, PgValueRef, Postgres,
};
use sqlx::{Decode, Type};

use super::{Date, Timestamp};
use crate::SignedDuration;

/// `2000-01-01T00:00:00Z`, Postgres's epoch, in Unix seconds.
const PG_EPOCH: jiff::Timestamp = jiff::Timestamp::constant(946_684_800, 0);
const PG_EPOCH_DATE: jiff::civil::Date = jiff::civil::date(2000, 1, 1);

/// A binary `timestamptz`: `micros` microseconds after `2000-01-01T00:00:00Z`. Postgres's
/// `infinity` (`i64::MAX`), `-infinity` (`i64::MIN`) and anything past jiff's range are errors.
pub fn decode_pg_binary_micros(micros: i64) -> Result<Timestamp, BoxDynError> {
    PG_EPOCH
        .checked_add(SignedDuration::from_micros(micros))
        .map(Timestamp)
        .map_err(|_| format!("timestamptz out of range: {micros} µs after 2000-01-01").into())
}

/// A text `timestamptz` as Postgres prints it with `DateStyle` ISO, in any session `TimeZone`
/// (`2026-09-30 12:00:00.5+00`, `2026-09-30 17:45:00+05:45`, `1900-01-01 05:41:16+05:41:16`).
/// The offset is applied; `infinity`, `BC` dates and years past 9999 are errors.
pub fn decode_pg_text(s: &str) -> Result<Timestamp, BoxDynError> {
    // jiff's ISO 8601 parser takes a space for `T`, `±HH`, `±HH:MM` and `±HH:MM:SS` offsets.
    let t: jiff::Timestamp = s
        .parse()
        .map_err(|e| format!("timestamptz {s:?} didn't parse: {e}"))?;
    Ok(Timestamp(t))
}

/// A text `date` as Postgres prints it with `DateStyle` ISO (`2026-09-30`).
pub fn decode_pg_date_text(s: &str) -> Result<Date, BoxDynError> {
    let d: jiff::civil::Date = s
        .parse()
        .map_err(|e| format!("date {s:?} didn't parse: {e}"))?;
    Ok(Date(d))
}

fn decode_pg_binary_days(days: i32) -> Result<Date, BoxDynError> {
    let span = jiff::Span::new().try_days(days)?;
    Ok(Date(PG_EPOCH_DATE.checked_add(span)?))
}

impl Type<Postgres> for Timestamp {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(Oid(1184))
    }
}

impl PgHasArrayType for Timestamp {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(Oid(1185))
    }
}

impl Encode<'_, Postgres> for Timestamp {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        jiff_sqlx::Timestamp::from(self.0).encode_by_ref(buf)
    }

    fn size_hint(&self) -> usize {
        std::mem::size_of::<i64>()
    }
}

impl<'r> Decode<'r, Postgres> for Timestamp {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            PgValueFormat::Binary => decode_pg_binary_micros(Decode::<Postgres>::decode(value)?),
            PgValueFormat::Text => decode_pg_text(value.as_str()?),
        }
    }
}

impl Type<Postgres> for Date {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(Oid(1082))
    }
}

impl PgHasArrayType for Date {
    fn array_type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(Oid(1182))
    }
}

impl Encode<'_, Postgres> for Date {
    fn encode_by_ref(&self, buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        jiff_sqlx::Date::from(self.0).encode_by_ref(buf)
    }

    fn size_hint(&self) -> usize {
        std::mem::size_of::<i32>()
    }
}

impl<'r> Decode<'r, Postgres> for Date {
    fn decode(value: PgValueRef<'r>) -> Result<Self, BoxDynError> {
        match value.format() {
            PgValueFormat::Binary => decode_pg_binary_days(Decode::<Postgres>::decode(value)?),
            PgValueFormat::Text => decode_pg_date_text(value.as_str()?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TimestampExt;

    fn ts(s: &str) -> Timestamp {
        Timestamp::parse_rfc3339(s).unwrap()
    }

    #[test]
    fn binary_micros_count_from_2000() {
        assert_eq!(
            decode_pg_binary_micros(0).unwrap(),
            ts("2000-01-01T00:00:00Z")
        );
        assert_eq!(
            decode_pg_binary_micros(-1).unwrap(),
            ts("1999-12-31T23:59:59.999999Z")
        );
        assert_eq!(
            decode_pg_binary_micros(844_084_800_500_000).unwrap(),
            ts("2026-09-30T12:00:00.5Z")
        );
    }

    #[test]
    fn binary_infinity_and_out_of_range_are_errors() {
        assert!(decode_pg_binary_micros(i64::MAX).is_err());
        assert!(decode_pg_binary_micros(i64::MIN).is_err());
        // Year 11500 (Postgres goes to 294276) is past jiff's range.
        assert!(decode_pg_binary_micros(300_000_000_000_000_000).is_err());
        assert!(decode_pg_binary_days(i32::MAX).is_err());
        assert!(decode_pg_binary_days(i32::MIN).is_err());
        assert_eq!(decode_pg_binary_days(-1).unwrap().to_string(), "1999-12-31");
    }

    #[test]
    fn text_in_any_session_time_zone_is_the_same_instant() {
        let t = ts("2026-09-30T12:00:00.5Z");
        for s in [
            "2026-09-30 12:00:00.5+00",
            "2026-09-30 17:45:00.5+05:45",
            "2026-09-30 09:30:00.5-02:30",
            "2026-09-30 21:00:00.5+09",
        ] {
            assert_eq!(decode_pg_text(s).unwrap(), t, "{s}");
        }
        // Local mean time before 1920 in Asia/Kathmandu: an offset with seconds.
        assert_eq!(
            decode_pg_text("1900-01-01 05:41:16+05:41:16").unwrap(),
            ts("1900-01-01T00:00:00Z")
        );
        assert_eq!(
            decode_pg_text("1999-12-31 23:59:59.999999+00").unwrap(),
            ts("1999-12-31T23:59:59.999999Z")
        );
    }

    #[test]
    fn text_infinity_and_bc_are_errors() {
        for s in [
            "infinity",
            "-infinity",
            "0044-03-15 00:00:00+00 BC",
            "10000-01-01 00:00:00+00",
            "",
        ] {
            assert!(decode_pg_text(s).is_err(), "{s:?} decoded");
        }
        for s in ["infinity", "-infinity", "0044-03-15 BC", ""] {
            assert!(decode_pg_date_text(s).is_err(), "{s:?} decoded");
        }
        assert_eq!(
            decode_pg_date_text("2026-09-30").unwrap().to_string(),
            "2026-09-30"
        );
    }
}
