//! One test per entry of the crate docs' "Differences from chrono": what the types do where
//! jiff can't do what chrono did. The Postgres `infinity` and offset-with-seconds entries are
//! decoder tests (`src/jiff_backed/pg.rs` unit tests, and `tests/pg_live.rs` against a server);
//! the range's ends in every constructor are `tests/range.rs`.

use std::collections::HashSet;

use chelabase_time::{Date, DateExt, Timestamp, TimestampExt};
use sqlx::postgres::Postgres;
use sqlx::{Type, TypeInfo};

fn ts(s: &str) -> Timestamp {
    Timestamp::parse_rfc3339(s).unwrap()
}

/// Range: a well-formed time past jiff's range is chrono's "input is out of range" from every
/// parser and from serde, marked as beyond the range; a malformed one is not.
#[test]
fn past_the_range_is_an_error_marked_beyond_range() {
    for s in [
        "9999-12-31T23:59:59.999999999Z",
        "9999-12-31T00:00:00Z",
        "9999-12-30T22:00:01Z",
        "9999-12-30T23:00:00+00:00",
    ] {
        for err in [
            Timestamp::parse_rfc3339(s).unwrap_err(),
            s.parse::<Timestamp>().unwrap_err(),
        ] {
            assert_eq!(err.to_string(), "input is out of range", "{s}");
            assert!(err.is_beyond_range(), "{s}");
        }
        let json = serde_json::Value::String(s.into());
        let err = serde_json::from_value::<Timestamp>(json).unwrap_err();
        assert_eq!(err.to_string(), "input is out of range", "{s}");
    }
    for s in ["+10000-01-01T00:00:00Z", "-10000-01-01T00:00:00Z"] {
        let err = s.parse::<Timestamp>().unwrap_err();
        assert!(err.is_beyond_range(), "{s}: {err}");
    }
    // Malformed, or invalid in any calendar: not beyond the range, whatever chrono called it.
    for s in [
        "2026-02-30T00:00:00Z",
        "2026-09-30T25:00:00Z",
        "2026-09-30T12:00:00+24:00",
        "2026-09-30T12:00:00",
        "nope",
        "99999999999999999999-01-01T00:00:00Z",
    ] {
        let err = s.parse::<Timestamp>().unwrap_err();
        assert!(!err.is_beyond_range(), "{s}: {err}");
    }
    // The last instant is in range and round-trips.
    assert_eq!(ts("9999-12-30T22:00:00.999999999Z"), Timestamp::MAX);
    assert_eq!(
        serde_json::to_string(&Timestamp::MAX).unwrap(),
        "\"9999-12-30T22:00:00.999999999Z\""
    );
    // Dates hold all of 9999; past it is beyond the range.
    assert_eq!(
        Date::parse_iso("9999-12-31").unwrap().to_string(),
        "9999-12-31"
    );
    let err = Date::parse_iso("+10000-01-01").unwrap_err();
    assert!(err.is_beyond_range());
    assert!(serde_json::from_str::<Date>("\"+10000-01-01\"").is_err());
}

/// Leap seconds: `23:59:60` reads as the next minute's second 0, the instant chrono's
/// `unix_nanos` and Postgres encode gave it; it is written back as that, and equals it.
#[test]
fn a_leap_second_is_the_next_minutes_second_zero() {
    for (leap, next) in [
        ("2026-12-31T23:59:60Z", "2027-01-01T00:00:00Z"),
        ("2026-12-31T23:59:60.5Z", "2027-01-01T00:00:00.500Z"),
        ("2026-12-31T23:59:60.5+01:00", "2026-12-31T23:00:00.500Z"),
    ] {
        let strict = Timestamp::parse_rfc3339(leap).unwrap();
        let relaxed: Timestamp = leap.parse().unwrap();
        let json: Timestamp =
            serde_json::from_value(serde_json::Value::String(leap.into())).unwrap();
        assert_eq!(strict, relaxed, "{leap}");
        assert_eq!(strict, json, "{leap}");
        assert_eq!(strict.to_rfc3339_auto(), next, "{leap}");
        assert_eq!(strict, ts(next), "{leap}");
    }
}

/// Postgres type names: the type infos are built from the OIDs, so `TypeInfo::name()` is `?`
/// (sqlx's own named constants are private). The OIDs are `tests/pg_types.rs`.
#[test]
fn type_info_names_are_unnamed() {
    assert_eq!(<Timestamp as Type<Postgres>>::type_info().name(), "?");
    assert_eq!(<Date as Type<Postgres>>::type_info().name(), "?");
}

/// `Hash` agrees with `Eq` (the values differ from chrono's, which nothing stored), and
/// `Debug` is the `to_rfc3339_auto` form.
#[test]
fn hash_agrees_with_eq_and_debug_is_rfc3339() {
    let set: HashSet<Timestamp> = [
        ts("2026-09-30T12:00:00Z"),
        ts("2026-09-30T14:00:00+02:00"),
        ts("2026-12-31T23:59:60Z"),
        ts("2027-01-01T00:00:00Z"),
    ]
    .into_iter()
    .collect();
    assert_eq!(set.len(), 2);
    let dates: HashSet<Date> = [
        Date::ymd(2026, 10, 1).unwrap(),
        "2026-10-01".parse().unwrap(),
    ]
    .into_iter()
    .collect();
    assert_eq!(dates.len(), 1);
    assert_eq!(
        format!("{:?}", ts("2026-09-30T12:00:00.5Z")),
        "2026-09-30T12:00:00.500Z"
    );
    assert_eq!(
        format!("{:?}", Date::ymd(2026, 10, 1).unwrap()),
        "2026-10-01"
    );
}
