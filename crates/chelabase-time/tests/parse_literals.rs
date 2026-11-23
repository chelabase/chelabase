//! Literals from the differential tests that compared these types with chrono while chrono was
//! in the tree: the strings chrono 0.4.45 accepted and rejected (through `FromStr` and serde),
//! the messages it gave for JSON of the wrong type, and the bytes sqlx's chrono encode bound
//! for instants with sub-microsecond digits. The types must keep doing exactly this.

use chelabase_time::{Date, Timestamp, TimestampExt};
use sqlx::Encode;
use sqlx::postgres::{PgArgumentBuffer, Postgres};

fn pg_bytes<'q, T: Encode<'q, Postgres>>(v: &T) -> Vec<u8> {
    let mut buf = PgArgumentBuffer::default();
    let _ = v.encode_by_ref(&mut buf).expect("encodes");
    buf.to_vec()
}

#[test]
fn deserialize_accepts_and_rejects_as_chrono_did() {
    let accepted = [
        "2026-09-30T12:00:00Z",
        "2026-09-30T14:00:00+02:00",
        "2026-09-30T06:30:00-05:30",
        "2026-09-30t12:00:00z",
        "2026-09-30 12:00:00Z",
        "2026-09-30T12:00:00.1Z",
        "2026-09-30T12:00:00.12Z",
        "2026-09-30T12:00:00.123Z",
        "2026-09-30T12:00:00.1234Z",
        "2026-09-30T12:00:00.12345Z",
        "2026-09-30T12:00:00.123456Z",
        "2026-09-30T12:00:00.1234567Z",
        "2026-09-30T12:00:00.12345678Z",
        "2026-09-30T12:00:00.123456789Z",
        "2026-09-30T12:00:00.1234567891Z",
        "2026-09-30T12:00:00.1234567891234567890123Z",
        // chrono's `FromStr` is relaxed: these are accepted by serde, not by `parse_rfc3339`.
        "2026-09-30T12:00:00+0200",
        "2026-09-30T12:00:00 +02:00",
        "2026-09-30T12:00:00UTC",
        "2026-09-30T12:00:00 utc",
        "2026-9-3T1:2:3Z",
        " 2026-09-30T12:00:00Z ",
        "2026 - 09 - 30T12 : 00 : 00Z",
        "+2026-09-30T12:00:00Z",
        "26-09-30T12:00:00Z",
        "2026-09-30T12:00:00−02:00",
        "2026-09-30T12:00:00+02::00",
        // A leap second: the same instant as the next minute's second 0.
        "2026-12-31T23:59:60Z",
        "2026-12-31T23:59:60.5+01:00",
    ];
    let rejected = [
        "",
        "2026-09-30T12:00:00",
        "2026-09-30",
        "2026-09-30T12:00Z",
        "2026-09-30T12:00:00+02",
        "2026-09-30T12:00:00.Z",
        "2026-09-31T12:00:00Z",
        "2026-09-30T24:00:00Z",
        "2026-09-30T12:60:00Z",
        "2026-09-30T12:00:61Z",
        "2026-09-30T12:00:00+24:00",
        "2026-09-30T12:00:00+02:60",
        "2026-09-30T12:00:00Zjunk",
        "2026-09-30T12:00:00GMT",
        "1790769600",
        "99999999999999999999-01-01T00:00:00Z",
        "2026-02-29T00:00:00Z",
        "2026-09-30TT12:00:00Z",
        "2026-09-30 T12:00:00Z",
    ];
    for s in accepted {
        assert!(s.parse::<Timestamp>().is_ok(), "rejects {s:?}");
        let json = serde_json::Value::String(s.into());
        assert!(
            serde_json::from_value::<Timestamp>(json).is_ok(),
            "serde rejects {s:?}"
        );
    }
    for s in rejected {
        assert!(s.parse::<Timestamp>().is_err(), "accepts {s:?}");
        let json = serde_json::Value::String(s.into());
        assert!(
            serde_json::from_value::<Timestamp>(json).is_err(),
            "serde accepts {s:?}"
        );
    }
}

/// chrono's messages for JSON of the wrong type, frozen.
#[test]
fn wrong_json_types_fail_with_chronos_messages() {
    for (v, at) in [
        ("1790769600", "integer `1790769600`"),
        ("null", "null"),
        ("true", "boolean `true`"),
        ("[]", "sequence"),
    ] {
        let column = if v == "[]" { 0 } else { v.len() };
        assert_eq!(
            serde_json::from_str::<Timestamp>(v)
                .unwrap_err()
                .to_string(),
            format!(
                "invalid type: {at}, expected an RFC 3339 formatted date and time string at line 1 column {column}"
            ),
            "{v}"
        );
        assert_eq!(
            serde_json::from_str::<Date>(v).unwrap_err().to_string(),
            format!(
                "invalid type: {at}, expected a formatted date string at line 1 column {column}"
            ),
            "{v}"
        );
    }
}

/// sqlx's chrono encode was `(t - 2000-01-01).num_microseconds()`: it cuts toward 2000, so half
/// a microsecond either side of 2000 is 0, and before 2000 it is not a floor.
#[test]
fn pre_2000_encode_matches_chrono_literal() {
    for (s, micros) in [
        ("1999-12-31T23:59:59.9999995Z", 0i64),
        ("2000-01-01T00:00:00.0000005Z", 0),
        ("1999-12-31T23:59:59.9999985Z", -1),
        ("2000-01-01T00:00:00.0000015Z", 1),
        ("1999-12-31T23:59:59Z", -1_000_000),
        ("1970-01-01T00:00:00.000000999Z", -946_684_799_999_999),
    ] {
        let expected = micros.to_be_bytes().to_vec();
        let t = Timestamp::parse_rfc3339(s).unwrap();
        assert_eq!(pg_bytes(&t), expected, "{s}");
    }
}
