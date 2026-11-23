//! The instants that round trip through a Postgres `timestamptz` (`Timestamp::PG_ROUND_TRIP_MIN`
//! to `Timestamp::PG_ROUND_TRIP_MAX`), and the Unix-time constructors over the whole range: every
//! instant the type holds, read back from its whole microseconds or milliseconds, is itself
//! truncated, the range's last second included.

use chelabase_time::{Timestamp, TimestampExt};
use proptest::prelude::*;

/// Read as a request body's time is (`FromStr`, which also takes a signed year).
fn ts(s: &str) -> Timestamp {
    s.parse().unwrap()
}

#[test]
fn the_round_trip_range_is_year_1_to_the_last_microsecond() {
    assert_eq!(
        Timestamp::PG_ROUND_TRIP_MIN.to_rfc3339_micros(),
        "0001-01-01T00:00:00.000000Z"
    );
    assert_eq!(
        Timestamp::PG_ROUND_TRIP_MAX.to_rfc3339_micros(),
        "9999-12-30T22:00:00.999999Z"
    );
    assert_eq!(Timestamp::PG_ROUND_TRIP_MAX, Timestamp::MAX.trunc_micros());
    assert_eq!(
        Timestamp::PG_ROUND_TRIP_MIN.timestamp_micros(),
        -62_135_596_800_000_000
    );
    assert_eq!(
        Timestamp::PG_ROUND_TRIP_MAX.timestamp_micros(),
        253_402_207_200_999_999
    );
}

#[test]
fn round_trips_through_pg_is_the_range_after_truncation() {
    for (s, fits) in [
        ("0001-01-01T00:00:00Z", true),
        ("0000-12-31T23:59:59.999999999Z", false),
        ("0000-01-01T00:00:00Z", false),
        ("-4713-11-24T00:00:00Z", false),
        ("-5000-01-01T00:00:00Z", false),
        ("2026-09-30T12:00:00.123456789Z", true),
        ("9999-12-30T22:00:00Z", true),
        ("9999-12-30T22:00:00.5Z", true),
        ("9999-12-30T22:00:00.999999Z", true),
        // Truncated to the microsecond Postgres keeps, the last nanoseconds are the last
        // microsecond.
        ("9999-12-30T22:00:00.999999999Z", true),
    ] {
        assert_eq!(ts(s).round_trips_through_pg(), fits, "{s}");
    }
    assert!(!Timestamp::MIN.round_trips_through_pg());
    assert!(Timestamp::MAX.round_trips_through_pg());
}

// N3 / #397: the constructors accept the parser's range, the last second included.
#[test]
fn from_unix_micros_and_millis_reach_the_last_second() {
    let last = Timestamp::PG_ROUND_TRIP_MAX.timestamp_micros();
    assert_eq!(
        Timestamp::from_unix_micros(last),
        Some(Timestamp::PG_ROUND_TRIP_MAX)
    );
    assert_eq!(Timestamp::from_unix_micros(last + 1), None);
    assert_eq!(
        Timestamp::from_unix_micros(253_402_207_200_500_000),
        Some(ts("9999-12-30T22:00:00.5Z"))
    );
    assert_eq!(
        Timestamp::from_unix_millis(253_402_207_200_999),
        Some(ts("9999-12-30T22:00:00.999Z"))
    );
    assert_eq!(Timestamp::from_unix_millis(253_402_207_201_000), None);
    // The low end is a whole second: one microsecond or millisecond before it is out.
    let first = Timestamp::MIN.timestamp_micros();
    assert_eq!(Timestamp::from_unix_micros(first), Some(Timestamp::MIN));
    assert_eq!(Timestamp::from_unix_micros(first - 1), None);
    let first = Timestamp::MIN.timestamp_millis();
    assert_eq!(Timestamp::from_unix_millis(first), Some(Timestamp::MIN));
    assert_eq!(Timestamp::from_unix_millis(first - 1), None);
}

/// Instants either side of the ends, to the nanosecond.
fn edges() -> Vec<Timestamp> {
    let mut out = Vec::new();
    for t in [
        Timestamp::MIN,
        Timestamp::MAX,
        Timestamp::PG_ROUND_TRIP_MIN,
        Timestamp::PG_ROUND_TRIP_MAX,
    ] {
        for d in [
            0u64,
            1,
            999,
            1_000,
            1_001,
            500_000_000,
            999_999_999,
            1_000_000_000,
        ] {
            let d = std::time::Duration::from_nanos(d);
            out.extend(t.checked_add(d));
            out.extend(t.checked_sub(d));
        }
    }
    out
}

fn assert_constructors_read_back(t: Timestamp) {
    assert_eq!(
        Timestamp::from_unix_micros(t.timestamp_micros()),
        Some(t.trunc_micros()),
        "{t:?}"
    );
    let millis = Timestamp::from_unix_millis(t.timestamp_millis()).unwrap();
    assert_eq!(millis.timestamp_millis(), t.timestamp_millis(), "{t:?}");
    assert!(millis <= t, "{t:?}");
}

#[test]
fn every_edge_instant_reads_back_from_its_micros_and_millis() {
    for t in edges() {
        assert_constructors_read_back(t);
    }
}

proptest! {
    #[test]
    fn any_instant_reads_back_from_its_micros_and_millis(
        nanos in Timestamp::MIN.unix_nanos()..=Timestamp::MAX.unix_nanos()
    ) {
        let t = Timestamp::from_jiff(jiff::Timestamp::from_nanosecond(nanos).unwrap());
        assert_constructors_read_back(t);
    }
}
