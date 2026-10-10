//! Caps and password-strength settings for login roles, read by the orchestrator and the worker.

use crate::mode::Mode;
use crate::mode_policy::ModePolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleSettings {
    pub role_cap: u32,
    pub database_cap: u32,
    pub strength: bool,
    pub prehashed_allowed: bool,
}

fn number(
    name: &str,
    get: &impl Fn(&str) -> Option<String>,
    default: u32,
    range: std::ops::RangeInclusive<u32>,
) -> Result<u32, String> {
    let Some(raw) = get(name).filter(|v| !v.trim().is_empty()) else {
        return Ok(default);
    };
    match raw.trim().parse::<u32>() {
        Ok(n) if range.contains(&n) => Ok(n),
        _ => Err(format!(
            "{name}={raw:?}: expected a number from {} to {}",
            range.start(),
            range.end()
        )),
    }
}

impl RoleSettings {
    /// Read `CHELABASE_BRANCH_ROLE_CAP` (50, 0..=500), `CHELABASE_BRANCH_DATABASE_CAP`
    /// (20, 1..=1000) and `CHELABASE_PASSWORD_STRENGTH` (default off). Blank is unset.
    /// Platform mode forces strength on and refuses pre-hashed passwords.
    pub fn from_lookup(mode: Mode, get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let role_cap = number("CHELABASE_BRANCH_ROLE_CAP", &get, 50, 0..=500)?;
        let database_cap = number("CHELABASE_BRANCH_DATABASE_CAP", &get, 20, 1..=1000)?;
        let strength = match get("CHELABASE_PASSWORD_STRENGTH")
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref()
        {
            None | Some("") | Some("off") | Some("false") | Some("0") => false,
            Some("on") | Some("true") | Some("1") => true,
            Some(other) => {
                return Err(format!(
                    "CHELABASE_PASSWORD_STRENGTH={other:?}: expected on or off"
                ));
            }
        };
        let policy = ModePolicy::new(mode);
        let forced = policy.password_strength_forced();
        Ok(Self {
            role_cap,
            database_cap,
            strength: strength || forced,
            prehashed_allowed: policy.prehashed_passwords_allowed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn defaults() {
        let s = RoleSettings::from_lookup(Mode::SelfHosted, lookup(&[])).unwrap();
        assert_eq!(
            s,
            RoleSettings {
                role_cap: 50,
                database_cap: 20,
                strength: false,
                prehashed_allowed: true
            }
        );
        let blank = [
            ("CHELABASE_BRANCH_ROLE_CAP", " "),
            ("CHELABASE_PASSWORD_STRENGTH", ""),
        ];
        assert_eq!(
            RoleSettings::from_lookup(Mode::SelfHosted, lookup(&blank)).unwrap(),
            s
        );
    }

    #[test]
    fn ranges() {
        let ok = |k: &str, v: &str| RoleSettings::from_lookup(Mode::SelfHosted, lookup(&[(k, v)]));
        assert_eq!(ok("CHELABASE_BRANCH_ROLE_CAP", "0").unwrap().role_cap, 0);
        assert_eq!(ok("CHELABASE_BRANCH_ROLE_CAP", "500").unwrap().role_cap, 500);
        assert!(
            ok("CHELABASE_BRANCH_ROLE_CAP", "501")
                .unwrap_err()
                .contains("CHELABASE_BRANCH_ROLE_CAP")
        );
        assert!(ok("CHELABASE_BRANCH_ROLE_CAP", "-1").is_err());
        assert!(ok("CHELABASE_BRANCH_ROLE_CAP", "x").is_err());
        assert_eq!(
            ok("CHELABASE_BRANCH_DATABASE_CAP", "1").unwrap().database_cap,
            1
        );
        assert_eq!(
            ok("CHELABASE_BRANCH_DATABASE_CAP", "1000")
                .unwrap()
                .database_cap,
            1000
        );
        assert!(
            ok("CHELABASE_BRANCH_DATABASE_CAP", "0")
                .unwrap_err()
                .contains("CHELABASE_BRANCH_DATABASE_CAP")
        );
        assert!(ok("CHELABASE_BRANCH_DATABASE_CAP", "1001").is_err());
    }

    #[test]
    fn strength_values() {
        let get = |v: &str| {
            RoleSettings::from_lookup(
                Mode::SelfHosted,
                lookup(&[("CHELABASE_PASSWORD_STRENGTH", v)]),
            )
        };
        for v in ["on", "ON", "true", "True", "1"] {
            assert!(get(v).unwrap().strength, "{v}");
        }
        for v in ["off", "Off", "false", "0"] {
            assert!(!get(v).unwrap().strength, "{v}");
        }
        assert!(
            get("maybe")
                .unwrap_err()
                .contains("CHELABASE_PASSWORD_STRENGTH")
        );
    }

    #[test]
    fn platform_forces_strength_and_refuses_prehashed() {
        let s = RoleSettings::from_lookup(
            Mode::Platform,
            lookup(&[("CHELABASE_PASSWORD_STRENGTH", "off")]),
        )
        .unwrap();
        assert!(s.strength && !s.prehashed_allowed);
        let d = RoleSettings::from_lookup(Mode::Platform, lookup(&[])).unwrap();
        assert!(d.strength && !d.prehashed_allowed);
    }
}
