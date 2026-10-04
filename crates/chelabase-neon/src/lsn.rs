//! Postgres log sequence numbers in their text form, e.g. `0/1F98488`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Lsn(pub u64);

impl FromStr for Lsn {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || format!("invalid LSN {s:?}; expected the form 0/1F98488");
        let (hi, lo) = s.split_once('/').ok_or_else(invalid)?;
        let part = |p: &str| {
            // from_str_radix alone would also take a leading '+'.
            if p.is_empty() || p.len() > 8 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid());
            }
            u32::from_str_radix(p, 16).map_err(|_| invalid())
        };
        Ok(Self((u64::from(part(hi)?) << 32) | u64::from(part(lo)?)))
    }
}

impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xffff_ffff)
    }
}

/// As the text form, the way Neon's APIs send and accept LSNs.
impl Serialize for Lsn {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Lsn {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::Lsn;

    #[test]
    fn parses_and_orders() {
        let a: Lsn = "0/1F98488".parse().unwrap();
        let b: Lsn = "1/0".parse().unwrap();
        assert_eq!(a.0, 0x1F98488);
        assert_eq!(b.0, 1 << 32);
        assert!(a < b);
        assert_eq!(a.to_string(), "0/1F98488");
        assert_eq!(
            "ff/ffffffff".parse::<Lsn>().unwrap().to_string(),
            "FF/FFFFFFFF"
        );
    }

    #[test]
    fn rejects_malformed() {
        for bad in [
            "",
            "0",
            "/1",
            "0/",
            "x/1",
            "0/1/2",
            "123456789/0",
            "-1/0",
            "+1/0",
            "0/+2",
            " 0/1",
        ] {
            assert!(bad.parse::<Lsn>().is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn serde_uses_the_text_form() {
        let lsn: Lsn = serde_json::from_str("\"0/14EE0B8\"").unwrap();
        assert_eq!(lsn.0, 0x14EE0B8);
        assert_eq!(serde_json::to_string(&lsn).unwrap(), "\"0/14EE0B8\"");
        assert_eq!(serde_json::from_str::<Option<Lsn>>("null").unwrap(), None);
        assert!(serde_json::from_str::<Lsn>("\"nope\"").is_err());
    }
}
