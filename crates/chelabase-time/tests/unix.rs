//! Unix-time constructors, truncation, arithmetic and the inherent methods the API keeps.
//! Every extension method is called here through the alias, so clippy (`--all-targets
//! -D warnings`) would flag one that resolved to a deprecated inherent chrono method.

use std::time::Duration;

use chelabase_time::{
    Date, DateExt, SignedDuration, Timestamp, TimestampExt, days, hours, minutes, seconds,
};

fn ts(s: &str) -> Timestamp {
    Timestamp::parse_rfc3339(s).unwrap()
}

#[test]
fn from_unix_micros_round_trips() {
    let t = Timestamp::from_unix_micros(1_900_000_000_123_456).unwrap();
    assert_eq!(t.timestamp_micros(), 1_900_000_000_123_456);
    assert_eq!(t.timestamp_millis(), 1_900_000_000_123);
    assert_eq!(t.timestamp(), 1_900_000_000);
    assert_eq!(t.to_rfc3339_auto(), "2030-03-17T17:46:40.123456Z");
}

#[test]
fn from_unix_seconds_and_millis() {
    assert_eq!(
        Timestamp::from_unix_seconds(0).unwrap(),
        Timestamp::UNIX_EPOCH
    );
    assert_eq!(
        Timestamp::from_unix_seconds(1_790_769_600).unwrap(),
        ts("2026-09-30T12:00:00Z")
    );
    assert_eq!(
        Timestamp::from_unix_millis(1_790_769_600_500).unwrap(),
        ts("2026-09-30T12:00:00.5Z")
    );
    assert_eq!(
        Timestamp::from_unix_micros(-1).unwrap().to_rfc3339_auto(),
        "1969-12-31T23:59:59.999999Z"
    );
    assert!(Timestamp::from_unix_seconds(i64::MAX).is_none());
    assert!(Timestamp::from_unix_millis(i64::MIN).is_none());
}

#[test]
fn from_ymd_hms_builds_utc() {
    assert_eq!(
        Timestamp::from_ymd_hms(2099, 1, 2, 3, 4, 5).unwrap(),
        ts("2099-01-02T03:04:05Z")
    );
    assert!(Timestamp::from_ymd_hms(2026, 2, 29, 0, 0, 0).is_none());
    assert!(Timestamp::from_ymd_hms(2026, 1, 1, 24, 0, 0).is_none());
}

#[test]
fn unix_nanos_is_exact() {
    assert_eq!(
        ts("2026-09-30T12:00:00.123456789Z").unix_nanos(),
        1_790_769_600_123_456_789
    );
    assert_eq!(ts("1969-12-31T23:59:59.999999999Z").unix_nanos(), -1);
    assert_eq!(Timestamp::UNIX_EPOCH.unix_nanos(), 0);
}

#[test]
fn trunc_micros_drops_never_rounds() {
    let t = ts("2026-09-30T12:00:00.123456789Z").trunc_micros();
    assert_eq!(t.to_rfc3339_auto(), "2026-09-30T12:00:00.123456Z");
    let t = ts("2026-09-30T12:00:00.999999999Z").trunc_micros();
    assert_eq!(t.to_rfc3339_auto(), "2026-09-30T12:00:00.999999Z");
    // Before 1970 it truncates toward the past too: the fraction counts forward.
    let t = ts("1969-12-31T23:59:59.999999999Z").trunc_micros();
    assert_eq!(t.to_rfc3339_auto(), "1969-12-31T23:59:59.999999Z");
    let t = ts("1999-12-31T23:59:59.000000999Z").trunc_micros();
    assert_eq!(t.to_rfc3339_auto(), "1999-12-31T23:59:59Z");
    let whole = ts("2026-09-30T12:00:00.123456Z");
    assert_eq!(whole.trunc_micros(), whole);
}

#[test]
fn since_is_signed() {
    let a = ts("2026-09-30T12:00:00Z");
    let b = ts("2026-09-30T12:00:01.5Z");
    assert_eq!(b.since(a), SignedDuration::from_millis(1500));
    assert_eq!(a.since(b), SignedDuration::from_millis(-1500));
    assert_eq!(a.since(a), SignedDuration::ZERO);
    assert_eq!(a.since(b).abs().as_millis(), 1500);
    assert!(b.since(a) >= SignedDuration::from_secs(1));
    assert_eq!(b.since(a).unsigned_abs(), Duration::from_millis(1500));
}

#[test]
fn checked_add_and_sub_take_std_durations() {
    let a = ts("2026-09-30T12:00:00Z");
    assert_eq!(a.checked_add(hours(1)).unwrap(), ts("2026-09-30T13:00:00Z"));
    assert_eq!(a.checked_sub(days(1)).unwrap(), ts("2026-09-29T12:00:00Z"));
    assert_eq!(
        a.checked_add(Duration::from_nanos(1))
            .unwrap()
            .to_rfc3339_auto(),
        "2026-09-30T12:00:00.000000001Z"
    );
    assert!(a.checked_add(Duration::MAX).is_none());
    assert!(a.checked_sub(Duration::MAX).is_none());
    assert!(a.checked_add(days(365 * 1_000_000)).is_none());
    // The operators take std durations too.
    assert_eq!(a + minutes(90), ts("2026-09-30T13:30:00Z"));
    assert_eq!(a - seconds(30), ts("2026-09-30T11:59:30Z"));
}

#[test]
fn now_is_utc_and_moves_forward() {
    let a = Timestamp::now();
    let b = Timestamp::now();
    assert!(b >= a);
    assert!(a > ts("2026-01-01T00:00:00Z"));
}

#[test]
fn duration_helpers_are_const() {
    const DAY: Duration = days(1);
    const H: Duration = hours(72);
    const M: Duration = minutes(10);
    const S: Duration = seconds(30);
    assert_eq!(DAY, Duration::from_secs(86_400));
    assert_eq!(H, Duration::from_secs(259_200));
    assert_eq!(M, Duration::from_secs(600));
    assert_eq!(S, Duration::from_secs(30));
}

#[test]
fn date_helpers() {
    let d = Date::ymd(2026, 10, 1).unwrap();
    assert_eq!(Date::parse_iso("2026-10-01").unwrap(), d);
    assert_eq!(
        Timestamp::from_ymd_hms(2026, 10, 1, 0, 0, 0)
            .unwrap()
            .utc_date(),
        d
    );
}

#[test]
fn converts_to_system_time() {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = ts("2026-09-30T12:00:00.123456789Z");
    let st = SystemTime::from(t);
    assert_eq!(
        st.duration_since(UNIX_EPOCH).unwrap(),
        Duration::new(1_790_769_600, 123_456_789)
    );
    let before = SystemTime::from(ts("1969-12-31T23:59:59Z"));
    assert_eq!(
        UNIX_EPOCH.duration_since(before).unwrap(),
        Duration::from_secs(1)
    );
}
