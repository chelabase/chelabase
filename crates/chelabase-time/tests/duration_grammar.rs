//! `parse_duration` keeps humantime 2.4.0's grammar: a frozen table of literals taken from it,
//! which outlived the differential test that compared the two while humantime was in the tree.

use std::time::Duration;

use chelabase_time::parse_duration;

const S: u64 = 1;
const MIN: u64 = 60;
const H: u64 = 3600;
const D: u64 = 86_400;
const W: u64 = 7 * D;
const MONTH: u64 = 2_630_016; // 30.44 days
const Y: u64 = 31_557_600; // 365.25 days

/// Every unit spelling humantime 2.4.0 accepts, with `3` of it.
const UNITS: &[(&str, Duration)] = &[
    ("nanos", Duration::from_nanos(3)),
    ("nsec", Duration::from_nanos(3)),
    ("ns", Duration::from_nanos(3)),
    ("usec", Duration::from_micros(3)),
    ("us", Duration::from_micros(3)),
    ("µs", Duration::from_micros(3)),
    ("millis", Duration::from_millis(3)),
    ("msec", Duration::from_millis(3)),
    ("ms", Duration::from_millis(3)),
    ("seconds", Duration::from_secs(3 * S)),
    ("second", Duration::from_secs(3 * S)),
    ("secs", Duration::from_secs(3 * S)),
    ("sec", Duration::from_secs(3 * S)),
    ("s", Duration::from_secs(3 * S)),
    ("minutes", Duration::from_secs(3 * MIN)),
    ("minute", Duration::from_secs(3 * MIN)),
    ("min", Duration::from_secs(3 * MIN)),
    ("mins", Duration::from_secs(3 * MIN)),
    ("m", Duration::from_secs(3 * MIN)),
    ("hours", Duration::from_secs(3 * H)),
    ("hour", Duration::from_secs(3 * H)),
    ("hr", Duration::from_secs(3 * H)),
    ("hrs", Duration::from_secs(3 * H)),
    ("h", Duration::from_secs(3 * H)),
    ("days", Duration::from_secs(3 * D)),
    ("day", Duration::from_secs(3 * D)),
    ("d", Duration::from_secs(3 * D)),
    ("weeks", Duration::from_secs(3 * W)),
    ("week", Duration::from_secs(3 * W)),
    ("wk", Duration::from_secs(3 * W)),
    ("wks", Duration::from_secs(3 * W)),
    ("w", Duration::from_secs(3 * W)),
    ("months", Duration::from_secs(3 * MONTH)),
    ("month", Duration::from_secs(3 * MONTH)),
    ("M", Duration::from_secs(3 * MONTH)),
    ("years", Duration::from_secs(3 * Y)),
    ("year", Duration::from_secs(3 * Y)),
    ("yr", Duration::from_secs(3 * Y)),
    ("yrs", Duration::from_secs(3 * Y)),
    ("y", Duration::from_secs(3 * Y)),
];

/// Combined and odd-but-accepted inputs, frozen.
const COMBINED: &[(&str, Duration)] = &[
    ("0", Duration::ZERO),
    ("0s", Duration::ZERO),
    ("1h 30m", Duration::from_secs(H + 30 * MIN)),
    ("1h30m", Duration::from_secs(H + 30 * MIN)),
    ("7days", Duration::from_secs(7 * D)),
    ("7 days", Duration::from_secs(7 * D)),
    ("604800s", Duration::from_secs(604_800)),
    (
        "1day 2h 3m 4s 5ms 6us 7ns",
        Duration::new(D + 2 * H + 3 * MIN + 4, 5_006_007),
    ),
    ("2M", Duration::from_secs(2 * MONTH)),
    ("1y", Duration::from_secs(Y)),
    ("1y 1M 1w 1d", Duration::from_secs(Y + MONTH + W + D)),
    ("  5m  ", Duration::from_secs(5 * MIN)),
    // Whitespace between digits is skipped: this is 12 seconds.
    ("1 2s", Duration::from_secs(12)),
    ("1.5h", Duration::from_secs(H + 30 * MIN)),
    ("0.5s", Duration::from_millis(500)),
    ("1.25d", Duration::from_secs(D + 6 * H)),
    ("1.5ms", Duration::from_micros(1500)),
    ("2.5us", Duration::from_nanos(2500)),
    ("0.1s", Duration::from_millis(100)),
    ("1.000000001s", Duration::new(1, 1)),
    ("1.5M", Duration::from_secs(MONTH + MONTH / 2)),
    ("1h 1h", Duration::from_secs(2 * H)),
    ("90m", Duration::from_secs(90 * MIN)),
    ("18446744073709551615s", Duration::from_secs(u64::MAX)),
    ("1000000000ns", Duration::from_secs(1)),
    ("1500ms", Duration::from_millis(1500)),
];

/// What humantime rejects, with its exact message.
const REJECTED: &[(&str, &str)] = &[
    ("", "value was empty"),
    ("   ", "value was empty"),
    ("5", "time unit needed, for example 5sec or 5ms"),
    ("10 ", "time unit needed, for example 10sec or 10ms"),
    (
        "5x",
        "unknown time unit \"x\", supported units: ns, us/µs, ms, sec, min, hours, days, weeks, \
         months, years (and few variations)",
    ),
    (
        "5 Sec",
        "unknown time unit \"Sec\", supported units: ns, us/µs, ms, sec, min, hours, days, weeks, \
         months, years (and few variations)",
    ),
    (
        "1H",
        "unknown time unit \"H\", supported units: ns, us/µs, ms, sec, min, hours, days, weeks, \
         months, years (and few variations)",
    ),
    ("s", "expected number at 0"),
    ("-1s", "expected number at 0"),
    ("1s x", "expected number at 3"),
    ("+1s", "expected number at 0"),
    ("1,5s", "invalid character at 1"),
    ("1s,", "invalid character at 2"),
    ("1.s", "invalid character at 1"),
    ("1.", "invalid character at 1"),
    ("1..5s", "invalid character at 1"),
    (
        "18446744073709551616s",
        "number is too large or cannot be represented without a lack of precision (values below \
         1ns are not supported)",
    ),
    (
        "18446744073709551615m",
        "number is too large or cannot be represented without a lack of precision (values below \
         1ns are not supported)",
    ),
    (
        "18446744073709551615s 1s",
        "number is too large or cannot be represented without a lack of precision (values below \
         1ns are not supported)",
    ),
    (
        "1.5ns",
        "number is too large or cannot be represented without a lack of precision (values below \
         1ns are not supported)",
    ),
    (
        "1.0001us",
        "number is too large or cannot be represented without a lack of precision (values below \
         1ns are not supported)",
    ),
    (
        "1.0000000001s",
        "number is too large or cannot be represented without a lack of precision (values below \
         1ns are not supported)",
    ),
];

#[test]
fn every_unit_parses_as_humantime_did() {
    for (unit, want) in UNITS {
        for input in [
            format!("3{unit}"),
            format!("3 {unit}"),
            format!(" 3{unit} "),
        ] {
            let got = parse_duration(&input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
            assert_eq!(got, *want, "{input:?} (frozen)");
        }
    }
}

#[test]
fn combined_units_parse_as_humantime_did() {
    for (input, want) in COMBINED {
        let got = parse_duration(input).unwrap_or_else(|e| panic!("{input:?}: {e}"));
        assert_eq!(got, *want, "{input:?} (frozen)");
    }
}

#[test]
fn rejects_what_humantime_rejected() {
    for (input, message) in REJECTED {
        let ours = parse_duration(input).expect_err(input);
        assert_eq!(ours.to_string(), *message, "{input:?} (frozen)");
    }
}

/// humantime panicked (in `Duration::new`) when a nanosecond carry landed on `u64::MAX` seconds;
/// ours returns the overflow error.
#[test]
fn a_carry_past_the_largest_duration_is_an_error_not_a_panic() {
    let input = "18446744073709551615s 999999999ns 1ns";
    assert!(parse_duration(input).is_err());
}
