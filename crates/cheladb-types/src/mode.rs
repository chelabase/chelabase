//! How Cheladb runs: `CHELADB_MODE=self-hosted` (the default, the dev mode) or
//! `platform` (the hosted cloud; not built yet).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    SelfHosted,
    Platform,
}

impl Mode {
    pub const ENV: &'static str = "CHELADB_MODE";

    /// Parse a `CHELADB_MODE` value; unset or blank is self-hosted.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("self-hosted") => Ok(Mode::SelfHosted),
            Some("platform") => Ok(Mode::Platform),
            Some(other) => Err(format!(
                "{}={other:?}: expected self-hosted or platform",
                Self::ENV
            )),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        Self::parse(std::env::var(Self::ENV).ok().as_deref())
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::SelfHosted => "self-hosted",
            Mode::Platform => "platform",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_hosted_is_the_default() {
        assert_eq!(Mode::parse(None).unwrap(), Mode::SelfHosted);
        assert_eq!(Mode::parse(Some("  ")).unwrap(), Mode::SelfHosted);
    }

    #[test]
    fn both_modes_parse_case_insensitively() {
        assert_eq!(Mode::parse(Some("self-hosted")).unwrap(), Mode::SelfHosted);
        assert_eq!(Mode::parse(Some(" Platform ")).unwrap(), Mode::Platform);
    }

    // A typo must not silently fall back to self-hosted.
    #[test]
    fn anything_else_is_an_error() {
        let err = Mode::parse(Some("cloud")).unwrap_err();
        assert!(
            err.contains("CHELADB_MODE") && err.contains("cloud"),
            "{err}"
        );
    }
}
