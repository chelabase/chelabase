//! Differential tests: the jiff backing (`chelabase_time::jiff_backed`) against chrono, the
//! backing it replaces, on random inputs. Deleted with chrono (epic drop-chrono, the switch
//! milestone); the literals in `pre_2000_encode_matches_chrono_literal` and the goldens stay.

use chelabase_time::jiff_backed::{Date as JDate, Timestamp as JTs};
use chelabase_time::{DateExt, SignedDuration, TimestampExt};
use chrono::{DateTime, NaiveDate, Utc};
use proptest::prelude::*;
use sqlx::Encode;
use sqlx::postgres::{PgArgumentBuffer, Postgres};

type CTs = DateTime<Utc>;

const NANOS: i128 = 1_000_000_000;
/// 1900-01-01T00:00:00Z and 2200-01-01T00:00:00Z.
const FROM_1900: i128 = -2_208_988_800 * NANOS;
const TO_2200: i128 = 7_258_118_400 * NANOS;
/// jiff's range, which is smaller than chrono's.
fn jiff_min() -> i128 {
    jiff::Timestamp::MIN.as_nanosecond()
}
fn jiff_max() -> i128 {
    jiff::Timestamp::MAX.as_nanosecond()
}

fn chrono_at(nanos: i128) -> CTs {
    let secs = nanos.div_euclid(NANOS) as i64;
    let sub = nanos.rem_euclid(NANOS) as u32;
    DateTime::from_timestamp(secs, sub).expect("in chrono's range")
}

fn jiff_at(nanos: i128) -> JTs {
    JTs::from_jiff(jiff::Timestamp::from_nanosecond(nanos).expect("in jiff's range"))
}

/// Nanoseconds in `range`, a third of them rounded to whole seconds, milliseconds or
/// microseconds so every fraction width is common.
fn instant(range: std::ops::Range<i128>) -> impl Strategy<Value = i128> {
    (range, 0..6u8).prop_map(|(n, precision)| {
        let unit = match precision {
            0 => NANOS,
            1 => 1_000_000,
            2 => 1_000,
            _ => 1,
        };
        n - n.rem_euclid(unit)
    })
}

fn pg_bytes<'q, T: Encode<'q, Postgres>>(v: &T) -> Vec<u8> {
    let mut buf = PgArgumentBuffer::default();
    let _ = v.encode_by_ref(&mut buf).expect("encodes");
    buf.to_vec()
}

fn assert_formats_agree(n: i128) {
    let (c, j) = (chrono_at(n), jiff_at(n));
    assert_eq!(j.to_rfc3339_auto(), c.to_rfc3339_auto(), "auto {n}");
    assert_eq!(j.to_rfc3339_millis(), c.to_rfc3339_millis(), "millis {n}");
    assert_eq!(j.to_rfc3339_micros(), c.to_rfc3339_micros(), "micros {n}");
    assert_eq!(j.to_rfc3339_offset(), c.to_rfc3339_offset(), "offset {n}");
    assert_eq!(j.to_iso8601_basic(), c.to_iso8601_basic(), "basic {n}");
    assert_eq!(
        j.utc_date().to_string(),
        c.utc_date().to_string(),
        "date {n}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4096))]

    #[test]
    fn json_matches_chrono(n in instant(FROM_1900..TO_2200)) {
        let (c, j) = (chrono_at(n), jiff_at(n));
        let json = serde_json::to_string(&c).unwrap();
        prop_assert_eq!(serde_json::to_string(&j).unwrap(), json.clone());
        let back: JTs = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, j);
    }

    #[test]
    fn formats_match_chrono(n in instant(FROM_1900..TO_2200)) {
        assert_formats_agree(n);
    }

    /// Outside 1900–2200 too, down to jiff's limits: negative years, years before 1000.
    #[test]
    fn formats_match_chrono_over_jiffs_range(n in instant(jiff_min()..jiff_max())) {
        assert_formats_agree(n);
        let (c, j) = (chrono_at(n), jiff_at(n));
        prop_assert_eq!(j.to_string(), c.to_string());
        prop_assert_eq!(serde_json::to_string(&j).unwrap(), serde_json::to_string(&c).unwrap());
    }

    #[test]
    fn display_matches_chrono(n in instant(FROM_1900..TO_2200)) {
        let (c, j) = (chrono_at(n), jiff_at(n));
        prop_assert_eq!(j.to_string(), c.to_string());
        prop_assert_eq!(format!("{j}"), format!("{c}"));
        let (cd, jd) = (c.utc_date(), j.utc_date());
        prop_assert_eq!(jd.to_string(), cd.to_string());
        prop_assert_eq!(format!("{jd}"), format!("{cd}"));
    }

    #[test]
    fn unix_conversions_match_chrono(n in instant(FROM_1900..TO_2200)) {
        let (c, j) = (chrono_at(n), jiff_at(n));
        prop_assert_eq!(j.timestamp(), c.timestamp());
        prop_assert_eq!(j.timestamp_millis(), c.timestamp_millis());
        prop_assert_eq!(j.timestamp_micros(), c.timestamp_micros());
        prop_assert_eq!(j.unix_nanos(), c.unix_nanos());
        prop_assert_eq!(j.trunc_micros().unix_nanos(), c.trunc_micros().unix_nanos());
        prop_assert_eq!(JTs::from_unix_seconds(c.timestamp()).unwrap().unix_nanos(),
            CTs::from_unix_seconds(c.timestamp()).unwrap().unix_nanos());
        prop_assert_eq!(JTs::from_unix_millis(c.timestamp_millis()).unwrap().unix_nanos(),
            CTs::from_unix_millis(c.timestamp_millis()).unwrap().unix_nanos());
        prop_assert_eq!(JTs::from_unix_micros(c.timestamp_micros()).unwrap().unix_nanos(),
            CTs::from_unix_micros(c.timestamp_micros()).unwrap().unix_nanos());
    }

    #[test]
    fn pg_bind_bytes_match_chrono(n in instant(FROM_1900..TO_2200)) {
        let (c, j) = (chrono_at(n), jiff_at(n));
        prop_assert_eq!(pg_bytes(&j), pg_bytes(&c));
        prop_assert_eq!(pg_bytes(&j.utc_date()), pg_bytes(&c.utc_date()));
        prop_assert_eq!(pg_bytes(&vec![j, j]), pg_bytes(&vec![c, c]));
        prop_assert_eq!(pg_bytes(&Some(j)), pg_bytes(&Some(c)));
    }

    #[test]
    fn subtraction_is_signed_and_matches_chrono(
        a in instant(FROM_1900..TO_2200),
        b in instant(FROM_1900..TO_2200),
    ) {
        let (ca, cb, ja, jb) = (chrono_at(a), chrono_at(b), jiff_at(a), jiff_at(b));
        let delta = ca - cb;
        let expected = SignedDuration::new(delta.num_seconds(), delta.subsec_nanos());
        prop_assert_eq!(ja - jb, expected);
        prop_assert_eq!(ja.since(jb), expected);
        prop_assert_eq!(ca.since(cb), expected);
        prop_assert_eq!((ja - jb).as_nanos(), a - b);
        prop_assert_eq!((jb - ja).as_nanos(), b - a);
        prop_assert_eq!(ja < jb, ca < cb);
        prop_assert_eq!(ja.cmp(&jb), ca.cmp(&cb));
    }

    #[test]
    fn std_duration_arithmetic_matches_chrono(
        n in instant(FROM_1900..TO_2200),
        d in 0u64..(100 * 365 * 86_400 * NANOS as u64),
    ) {
        let d = std::time::Duration::from_nanos(d);
        let (c, j) = (chrono_at(n), jiff_at(n));
        prop_assert_eq!((j + d).unix_nanos(), (c + d).unix_nanos());
        prop_assert_eq!((j - d).unix_nanos(), (c - d).unix_nanos());
        prop_assert_eq!(j.checked_add(d).map(|t| t.unix_nanos()), c.checked_add(d).map(|t| t.unix_nanos()));
        prop_assert_eq!(j.checked_sub(d).map(|t| t.unix_nanos()), c.checked_sub(d).map(|t| t.unix_nanos()));
    }

    #[test]
    fn dates_match_chrono(days in -719_162i64..=2_932_896) {
        // Every date from 0001-01-01 to 9999-12-31.
        let c = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap() + chrono::TimeDelta::days(days);
        let j = JDate::parse_iso(&c.to_string()).unwrap();
        prop_assert_eq!(j.to_string(), c.to_string());
        prop_assert_eq!(serde_json::to_string(&j).unwrap(), serde_json::to_string(&c).unwrap());
        prop_assert_eq!(pg_bytes(&j), pg_bytes(&c));
        prop_assert_eq!(pg_bytes(&vec![j]), pg_bytes(&vec![c]));
        use chrono::Datelike;
        let built = JDate::ymd(c.year(), c.month(), c.day()).unwrap();
        prop_assert_eq!(built, j);
    }
}

/// What a parse returned, comparable across the backings: the instant, or chrono's message.
fn outcome<T, E: std::fmt::Display>(
    r: Result<T, E>,
    nanos: impl Fn(&T) -> i128,
) -> Result<i128, String> {
    r.as_ref().map(nanos).map_err(|e| e.to_string())
}

/// Every parse the backings expose, on one string: either the same result, or chrono accepted
/// an instant past jiff's range (or a year past 9999), which the jiff backing reports as out
/// of range.
fn assert_parses_agree(s: &str) {
    let in_range = |r: &Result<i128, String>| match r {
        Ok(n) => (jiff_min()..=jiff_max()).contains(n),
        Err(_) => true,
    };
    let pairs = [
        (
            "parse_rfc3339",
            outcome(CTs::parse_rfc3339(s), |t| t.unix_nanos()),
            outcome(JTs::parse_rfc3339(s), |t| t.unix_nanos()),
        ),
        (
            "FromStr",
            outcome(s.parse::<CTs>(), |t| t.unix_nanos()),
            outcome(s.parse::<JTs>(), |t| t.unix_nanos()),
        ),
        (
            "serde",
            outcome(
                serde_json::from_value::<CTs>(serde_json::Value::String(s.into())),
                |t| t.unix_nanos(),
            ),
            outcome(
                serde_json::from_value::<JTs>(serde_json::Value::String(s.into())),
                |t| t.unix_nanos(),
            ),
        ),
    ];
    for (what, c, j) in pairs {
        if in_range(&c) {
            assert_eq!(j, c, "{what} of {s:?}");
        } else {
            assert_eq!(j, Err("input is out of range".into()), "{what} of {s:?}");
        }
    }
    let date_pairs = [
        (
            "parse_iso",
            NaiveDate::parse_iso(s)
                .map(|d| d.to_string())
                .map_err(|e| e.to_string()),
            JDate::parse_iso(s)
                .map(|d| d.to_string())
                .map_err(|e| e.to_string()),
        ),
        (
            "Date FromStr",
            s.parse::<NaiveDate>()
                .map(|d| d.to_string())
                .map_err(|e| e.to_string()),
            s.parse::<JDate>()
                .map(|d| d.to_string())
                .map_err(|e| e.to_string()),
        ),
    ];
    for (what, c, j) in date_pairs {
        let past_jiff = matches!(&c, Ok(d) if d.starts_with('+') || d.len() > 11);
        if past_jiff {
            assert_eq!(j, Err("input is out of range".into()), "{what} of {s:?}");
        } else {
            assert_eq!(j, c, "{what} of {s:?}");
        }
    }
}

#[test]
fn deserialize_accepts_and_rejects_like_chrono() {
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
        assert!(s.parse::<CTs>().is_ok(), "chrono rejects {s:?}");
        assert!(s.parse::<JTs>().is_ok(), "jiff rejects {s:?}");
        assert_parses_agree(s);
    }
    for s in rejected {
        assert!(s.parse::<CTs>().is_err(), "chrono accepts {s:?}");
        assert!(s.parse::<JTs>().is_err(), "jiff accepts {s:?}");
        assert_parses_agree(s);
    }
    // Wrong JSON types fail with chrono's message.
    for v in ["1790769600", "null", "true", "[]"] {
        assert_eq!(
            serde_json::from_str::<JTs>(v).unwrap_err().to_string(),
            serde_json::from_str::<CTs>(v).unwrap_err().to_string(),
            "{v}"
        );
        assert_eq!(
            serde_json::from_str::<JDate>(v).unwrap_err().to_string(),
            serde_json::from_str::<NaiveDate>(v)
                .unwrap_err()
                .to_string(),
            "{v}"
        );
    }
}

#[test]
fn leap_second_is_the_next_seconds_instant() {
    let j: JTs = "2026-12-31T23:59:60.5Z".parse().unwrap();
    let c: CTs = "2026-12-31T23:59:60.5Z".parse().unwrap();
    assert_eq!(j.unix_nanos(), c.unix_nanos());
    assert_eq!(pg_bytes(&j), pg_bytes(&c));
    // chrono keeps the `:60`; jiff can't represent it (see the crate docs, "Differences").
    assert_eq!(c.to_rfc3339_auto(), "2026-12-31T23:59:60.500Z");
    assert_eq!(j.to_rfc3339_auto(), "2027-01-01T00:00:00.500Z");
}

/// Strings built from the characters the grammars care about, so most of them are near misses.
fn near_rfc3339() -> impl Strategy<Value = String> {
    let piece = prop_oneof![
        Just("2026".to_string()),
        Just("0999".to_string()),
        Just("9999".to_string()),
        Just("12".to_string()),
        Just("02".to_string()),
        Just("29".to_string()),
        Just("31".to_string()),
        Just("60".to_string()),
        Just("23".to_string()),
        "[0-9]{1,3}",
        Just("-".to_string()),
        Just(":".to_string()),
        Just("T".to_string()),
        Just("t".to_string()),
        Just(" ".to_string()),
        Just("\t".to_string()),
        Just(".".to_string()),
        Just("Z".to_string()),
        Just("z".to_string()),
        Just("+".to_string()),
        Just("−".to_string()),
        Just("UTC".to_string()),
        Just("utc".to_string()),
        Just("\u{a0}".to_string()),
    ];
    prop::collection::vec(piece, 0..16).prop_map(|v| v.concat())
}

/// Valid RFC 3339 strings with one random edit: a character removed, doubled or replaced.
fn mutated_rfc3339() -> impl Strategy<Value = String> {
    (
        instant(jiff_min()..jiff_max()),
        -1439i32..=1439,
        0usize..40,
        0u8..4,
        prop::sample::select(vec![' ', '0', '9', ':', '-', '+', 'T', 'Z', '.', '\n']),
    )
        .prop_map(|(n, offset_min, at, op, ch)| {
            let c = chrono_at(n)
                .with_timezone(&chrono::FixedOffset::east_opt(offset_min * 60).unwrap());
            let mut chars: Vec<char> = c.to_rfc3339().chars().collect();
            let at = at.min(chars.len().saturating_sub(1));
            match op {
                0 if !chars.is_empty() => {
                    chars.remove(at);
                }
                1 if !chars.is_empty() => chars.insert(at, chars[at]),
                2 if !chars.is_empty() => chars[at] = ch,
                _ => chars.insert(at, ch),
            }
            chars.into_iter().collect()
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(8_000))]

    #[test]
    fn near_misses_parse_like_chrono(s in near_rfc3339()) {
        assert_parses_agree(&s);
    }

    #[test]
    fn edited_timestamps_parse_like_chrono(s in mutated_rfc3339()) {
        assert_parses_agree(&s);
    }

    #[test]
    fn any_offset_parses_like_chrono(n in instant(jiff_min()..jiff_max()), offset_min in -1439i32..=1439) {
        let c = chrono_at(n).with_timezone(&chrono::FixedOffset::east_opt(offset_min * 60).unwrap());
        assert_parses_agree(&c.to_rfc3339());
        assert_parses_agree(&c.to_string());
    }
}

/// sqlx's chrono encode is `(t - 2000-01-01).num_microseconds()`: it cuts toward 2000, so half
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
        let c = CTs::parse_rfc3339(s).unwrap();
        let j = JTs::parse_rfc3339(s).unwrap();
        assert_eq!(pg_bytes(&c), expected, "chrono {s}");
        assert_eq!(pg_bytes(&j), expected, "jiff {s}");
    }
}
