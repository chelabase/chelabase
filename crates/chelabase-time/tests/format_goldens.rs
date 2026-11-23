//! Today's timestamp and date formats, frozen as literals. They were taken from the chrono
//! backing; whatever backs `Timestamp` and `Date` later must write and read exactly these.

use chelabase_time::{Date, DateExt, Timestamp, TimestampExt};

fn ts(s: &str) -> Timestamp {
    Timestamp::parse_rfc3339(s).unwrap_or_else(|e| panic!("{s}: {e}"))
}

/// One row per fraction width: 0, 1–3, 4–6 and 7–9 significant digits.
struct Row {
    input: &'static str,
    serde: &'static str,
    auto: &'static str,
    millis: &'static str,
    micros: &'static str,
    offset: &'static str,
    display: &'static str,
}

const ROWS: &[Row] = &[
    Row {
        input: "2026-09-30T12:00:00Z",
        serde: "\"2026-09-30T12:00:00Z\"",
        auto: "2026-09-30T12:00:00Z",
        millis: "2026-09-30T12:00:00.000Z",
        micros: "2026-09-30T12:00:00.000000Z",
        offset: "2026-09-30T12:00:00+00:00",
        display: "2026-09-30 12:00:00 UTC",
    },
    Row {
        input: "2026-09-30T12:00:00.5Z",
        serde: "\"2026-09-30T12:00:00.500Z\"",
        auto: "2026-09-30T12:00:00.500Z",
        millis: "2026-09-30T12:00:00.500Z",
        micros: "2026-09-30T12:00:00.500000Z",
        offset: "2026-09-30T12:00:00.500+00:00",
        display: "2026-09-30 12:00:00.500 UTC",
    },
    Row {
        input: "2026-09-30T12:00:00.123Z",
        serde: "\"2026-09-30T12:00:00.123Z\"",
        auto: "2026-09-30T12:00:00.123Z",
        millis: "2026-09-30T12:00:00.123Z",
        micros: "2026-09-30T12:00:00.123000Z",
        offset: "2026-09-30T12:00:00.123+00:00",
        display: "2026-09-30 12:00:00.123 UTC",
    },
    Row {
        input: "2026-09-30T12:00:00.1234Z",
        serde: "\"2026-09-30T12:00:00.123400Z\"",
        auto: "2026-09-30T12:00:00.123400Z",
        millis: "2026-09-30T12:00:00.123Z",
        micros: "2026-09-30T12:00:00.123400Z",
        offset: "2026-09-30T12:00:00.123400+00:00",
        display: "2026-09-30 12:00:00.123400 UTC",
    },
    Row {
        input: "2026-09-30T12:00:00.123456Z",
        serde: "\"2026-09-30T12:00:00.123456Z\"",
        auto: "2026-09-30T12:00:00.123456Z",
        millis: "2026-09-30T12:00:00.123Z",
        micros: "2026-09-30T12:00:00.123456Z",
        offset: "2026-09-30T12:00:00.123456+00:00",
        display: "2026-09-30 12:00:00.123456 UTC",
    },
    Row {
        input: "2026-09-30T12:00:00.1234567Z",
        serde: "\"2026-09-30T12:00:00.123456700Z\"",
        auto: "2026-09-30T12:00:00.123456700Z",
        millis: "2026-09-30T12:00:00.123Z",
        micros: "2026-09-30T12:00:00.123456Z",
        offset: "2026-09-30T12:00:00.123456700+00:00",
        display: "2026-09-30 12:00:00.123456700 UTC",
    },
    Row {
        input: "2026-09-30T12:00:00.123456789Z",
        serde: "\"2026-09-30T12:00:00.123456789Z\"",
        auto: "2026-09-30T12:00:00.123456789Z",
        millis: "2026-09-30T12:00:00.123Z",
        micros: "2026-09-30T12:00:00.123456Z",
        offset: "2026-09-30T12:00:00.123456789+00:00",
        display: "2026-09-30 12:00:00.123456789 UTC",
    },
    // Millis and micros truncate, never round.
    Row {
        input: "2026-09-30T12:00:00.999999999Z",
        serde: "\"2026-09-30T12:00:00.999999999Z\"",
        auto: "2026-09-30T12:00:00.999999999Z",
        millis: "2026-09-30T12:00:00.999Z",
        micros: "2026-09-30T12:00:00.999999Z",
        offset: "2026-09-30T12:00:00.999999999+00:00",
        display: "2026-09-30 12:00:00.999999999 UTC",
    },
    // Before 1970 and before 1000: four-digit years, fraction counted forward from the second.
    Row {
        input: "1969-12-31T23:59:59.000001Z",
        serde: "\"1969-12-31T23:59:59.000001Z\"",
        auto: "1969-12-31T23:59:59.000001Z",
        millis: "1969-12-31T23:59:59.000Z",
        micros: "1969-12-31T23:59:59.000001Z",
        offset: "1969-12-31T23:59:59.000001+00:00",
        display: "1969-12-31 23:59:59.000001 UTC",
    },
    Row {
        input: "0999-01-02T03:04:05Z",
        serde: "\"0999-01-02T03:04:05Z\"",
        auto: "0999-01-02T03:04:05Z",
        millis: "0999-01-02T03:04:05.000Z",
        micros: "0999-01-02T03:04:05.000000Z",
        offset: "0999-01-02T03:04:05+00:00",
        display: "0999-01-02 03:04:05 UTC",
    },
];

#[test]
fn every_format_matches_its_golden() {
    for r in ROWS {
        let t = ts(r.input);
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            r.serde,
            "serde of {}",
            r.input
        );
        assert_eq!(t.to_rfc3339_auto(), r.auto, "auto of {}", r.input);
        assert_eq!(t.to_rfc3339_millis(), r.millis, "millis of {}", r.input);
        assert_eq!(t.to_rfc3339_micros(), r.micros, "micros of {}", r.input);
        assert_eq!(t.to_rfc3339_offset(), r.offset, "offset of {}", r.input);
        assert_eq!(t.to_string(), r.display, "Display of {}", r.input);
        assert_eq!(format!("{t}"), r.display, "{{}} of {}", r.input);
    }
}

#[test]
fn serde_reads_back_what_it_writes() {
    for r in ROWS {
        let back: Timestamp = serde_json::from_str(r.serde).unwrap();
        assert_eq!(back, ts(r.input), "{}", r.serde);
    }
}

#[test]
fn basic_iso8601_is_whole_seconds_without_separators() {
    assert_eq!(
        ts("2026-09-30T12:00:00Z").to_iso8601_basic(),
        "20260930T120000Z"
    );
    assert_eq!(
        ts("2026-01-02T03:04:05.999Z").to_iso8601_basic(),
        "20260102T030405Z"
    );
}

#[test]
fn parse_rfc3339_takes_z_and_offsets_and_returns_utc() {
    let utc = ts("2026-09-30T12:00:00Z");
    for s in [
        "2026-09-30T12:00:00Z",
        "2026-09-30T12:00:00z",
        "2026-09-30t12:00:00Z",
        "2026-09-30 12:00:00Z",
        "2026-09-30T12:00:00+00:00",
        "2026-09-30T12:00:00-00:00",
        "2026-09-30T14:00:00+02:00",
        "2026-09-30T06:30:00-05:30",
        "2026-10-01T11:59:00+23:59",
    ] {
        assert_eq!(ts(s), utc, "{s}");
        assert_eq!(ts(s).to_rfc3339_auto(), "2026-09-30T12:00:00Z", "{s}");
    }
}

#[test]
fn parse_rfc3339_reads_any_fraction_width_and_drops_digits_past_nine() {
    assert_eq!(
        ts("2026-09-30T12:00:00.1Z").to_rfc3339_auto(),
        "2026-09-30T12:00:00.100Z"
    );
    assert_eq!(
        ts("2026-09-30T12:00:00.1234567891Z").to_rfc3339_auto(),
        "2026-09-30T12:00:00.123456789Z"
    );
}

#[test]
fn parse_rfc3339_rejects_what_is_not_rfc3339() {
    for s in [
        "",
        "2026-09-30T12:00:00",
        "2026-09-30T12:00:00.5",
        "2026-09-30",
        "2026-09-30T12:00Z",
        "2026-09-30T12:00:00 Z",
        "2026-09-30T12:00:00+0200",
        "2026-09-30T12:00:00+02",
        "2026-09-30T12:00:00UTC",
        "2026-09-30T12:00:00.Z",
        "2026-9-30T12:00:00Z",
        "2026-09-31T12:00:00Z",
        "2026-09-30T24:00:00Z",
        "1790769600",
        " 2026-09-30T12:00:00Z",
        "2026-09-30T12:00:00Z ",
    ] {
        assert!(Timestamp::parse_rfc3339(s).is_err(), "{s:?} parsed");
    }
}

#[test]
fn serde_reads_offsets_as_utc() {
    let t: Timestamp = serde_json::from_str("\"2026-09-30T14:00:00+02:00\"").unwrap();
    assert_eq!(t, ts("2026-09-30T12:00:00Z"));
    assert!(serde_json::from_str::<Timestamp>("\"2026-09-30T12:00:00\"").is_err());
    assert!(serde_json::from_str::<Timestamp>("1790769600").is_err());
}

#[test]
fn date_formats_match_their_goldens() {
    let d = Date::ymd(2026, 10, 1).unwrap();
    assert_eq!(serde_json::to_string(&d).unwrap(), "\"2026-10-01\"");
    assert_eq!(d.to_string(), "2026-10-01");
    assert_eq!(serde_json::from_str::<Date>("\"2026-10-01\"").unwrap(), d);
    assert_eq!(Date::ymd(999, 1, 2).unwrap().to_string(), "0999-01-02");
    assert_eq!(
        ts("2026-09-30T23:59:59.999Z").utc_date(),
        Date::ymd(2026, 9, 30).unwrap()
    );
    assert_eq!(
        ts("2026-10-01T01:00:00+02:00").utc_date(),
        Date::ymd(2026, 9, 30).unwrap()
    );
}

#[test]
fn parse_iso_reads_year_month_day() {
    let d = Date::ymd(2026, 10, 1).unwrap();
    assert_eq!(Date::parse_iso("2026-10-01").unwrap(), d);
    // `%Y-%m-%d` today also takes one-digit months and days and a signed year.
    assert_eq!(
        Date::parse_iso("2026-1-5").unwrap(),
        Date::ymd(2026, 1, 5).unwrap()
    );
    assert_eq!(Date::parse_iso("+2026-10-01").unwrap(), d);
    // Leading whitespace is skipped before the year; trailing is not.
    assert_eq!(Date::parse_iso(" 2026-10-01").unwrap(), d);
    for s in [
        "",
        "2026-10",
        "2026-13-01",
        "2026-02-30",
        "2026/10/01",
        "20261001",
        "2026-10-01T00:00:00Z",
        "2026-10-01 ",
    ] {
        assert!(Date::parse_iso(s).is_err(), "{s:?} parsed");
    }
}

#[test]
fn ymd_refuses_a_day_that_does_not_exist() {
    assert!(Date::ymd(2026, 2, 29).is_none());
    assert!(Date::ymd(2028, 2, 29).is_some());
    assert!(Date::ymd(2026, 0, 1).is_none());
    assert!(Date::ymd(2026, 1, 32).is_none());
}
