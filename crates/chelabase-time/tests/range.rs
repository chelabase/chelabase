//! The ends of the types' range (jiff's): in range, or `None`/an error, never a panic and never
//! a clamped value. jiff's UTC range is about -9999-01-02T01:59:59Z to 9999-12-30T22:00:00Z.

use chelabase_time::{Date, DateExt, Timestamp, TimestampExt};

#[test]
fn extremes_error_and_never_panic() {
    // The largest RFC 3339 instant chrono reads is past jiff's maximum: an error, not a clamp.
    for s in [
        "9999-12-31T23:59:59.999999999Z",
        "9999-12-31T00:00:00Z",
        "9999-12-30T22:00:01Z",
        "+10000-01-01T00:00:00Z",
        "-10000-01-01T00:00:00Z",
    ] {
        let strict = Timestamp::parse_rfc3339(s);
        let relaxed = s.parse::<Timestamp>();
        let serde = serde_json::from_value::<Timestamp>(serde_json::Value::String(s.into()));
        assert!(relaxed.is_err(), "{s:?} parsed");
        assert_eq!(
            relaxed.unwrap_err().to_string(),
            "input is out of range",
            "{s}"
        );
        assert!(serde.is_err(), "{s:?} deserialized");
        if s.len() >= 20 && !s.starts_with(['+', '-']) {
            assert_eq!(strict.unwrap_err().to_string(), "input is out of range");
        }
    }
    // The low end of the RFC 3339 grammar is in range.
    let low = Timestamp::parse_rfc3339("0001-01-01T00:00:00Z").unwrap();
    assert_eq!(low.timestamp(), -62_135_596_800);
    assert_eq!(low.to_rfc3339_auto(), "0001-01-01T00:00:00Z");
    let high = Timestamp::parse_rfc3339("9999-12-30T22:00:00Z").unwrap();
    assert_eq!(high.to_rfc3339_auto(), "9999-12-30T22:00:00Z");
    assert_eq!(high.to_string(), "9999-12-30 22:00:00 UTC");

    // Constructors return `None` past the range.
    assert!(Timestamp::from_unix_seconds(i64::MAX).is_none());
    assert!(Timestamp::from_unix_seconds(i64::MIN).is_none());
    assert!(Timestamp::from_unix_millis(i64::MIN).is_none());
    assert!(Timestamp::from_unix_millis(i64::MAX).is_none());
    // Every i64 of microseconds is ±292,000 years: past the range at both ends.
    assert!(Timestamp::from_unix_micros(i64::MAX).is_none());
    assert!(Timestamp::from_unix_micros(i64::MIN).is_none());
    assert!(Timestamp::from_unix_seconds(253_402_207_200).is_some());
    assert!(Timestamp::from_unix_seconds(253_402_207_201).is_none());
    assert!(Timestamp::from_ymd_hms(9999, 12, 31, 23, 59, 59).is_none());
    assert!(Timestamp::from_ymd_hms(9999, 12, 30, 22, 0, 0).is_some());
    assert!(Timestamp::from_ymd_hms(10_000, 1, 1, 0, 0, 0).is_none());
    assert!(Timestamp::from_ymd_hms(i32::MAX, u32::MAX, 1, 0, 0, 0).is_none());
    assert!(Timestamp::from_ymd_hms(2026, 1, 1, 0, 0, 60).is_none());

    // Arithmetic past the range is `None` from the checked forms.
    assert!(
        high.checked_add(std::time::Duration::from_secs(1))
            .is_none()
    );
    assert!(low.checked_sub(std::time::Duration::MAX).is_none());

    // The type's own ends format, truncate and give a date without panicking.
    let (min, max) = (Timestamp::MIN, Timestamp::MAX);
    assert_eq!(min, Timestamp::from_jiff(jiff::Timestamp::MIN));
    assert_eq!(max, Timestamp::from_jiff(jiff::Timestamp::MAX));
    assert_eq!(min.to_rfc3339_auto(), "-9999-01-02T01:59:59Z");
    assert_eq!(max.to_rfc3339_auto(), "9999-12-30T22:00:00.999999999Z");
    assert_eq!(min.to_string(), "-9999-01-02 01:59:59 UTC");
    assert_eq!(min.trunc_micros(), min);
    assert_eq!(
        max.trunc_micros().to_rfc3339_auto(),
        "9999-12-30T22:00:00.999999Z"
    );
    assert_eq!(min.utc_date().to_string(), "-9999-01-02");
    assert_eq!(max.utc_date().to_string(), "9999-12-30");
    assert_eq!(min.to_iso8601_basic(), "-99990102T015959Z");
    assert!(max.since(min).as_secs() > 0);
    assert!(min.since(max).as_secs() < 0);

    // Dates: jiff's civil range is -9999..=9999, all of 9999 included.
    assert_eq!(Date::ymd(9999, 12, 31).unwrap().to_string(), "9999-12-31");
    assert_eq!(Date::ymd(-9999, 1, 1).unwrap().to_string(), "-9999-01-01");
    assert!(Date::ymd(10_000, 1, 1).is_none());
    assert!(Date::ymd(i32::MIN, 1, 1).is_none());
    assert_eq!(
        Date::parse_iso("+10000-01-01").unwrap_err().to_string(),
        "input is out of range"
    );
    // Without a sign the year is four digits at most, as in chrono.
    assert_eq!(
        "10000-01-01".parse::<Date>().unwrap_err().to_string(),
        "input contains invalid characters"
    );
}
