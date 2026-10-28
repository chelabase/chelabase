//! `CHELABASE_DEV_DEFAULTS=1`: the workbench's switch that lets a self-hosted service start
//! with the public development bearers (the fixed `chelabase-dev-*` values in the repository).
//! Without it, a self-hosted service refuses them at startup like a platform one does: one
//! wiring slip in compose or k8s would otherwise leave the internal API open to anyone who
//! read the source. Platform mode refuses them whatever this says.

use crate::mode_policy::ModePolicy;

/// Every public development bearer, by its variable: what a service uses when the variable
/// (and its `_FILE`) is unset. The values are in this repository, so each is refused outside
/// the workbench ([`DevDefaults::check_bearers`]).
pub const DEV_BEARERS: [(&str, &str); 7] = [
    ("CHELABASE_PROXY_API_TOKEN", "chelabase-dev-proxy-api"),
    ("CHELABASE_STORCON_HOOK_TOKEN", "chelabase-dev-storcon-hook"),
    ("CHELABASE_DATA_KEYS_TOKEN", "chelabase-dev-data-keys"),
    ("CHELABASE_REST_INTERNAL_TOKEN", "chelabase-dev-rest-internal"),
    ("CHELABASE_REALTIME_INTERNAL_TOKEN", "chelabase-dev-realtime-internal"),
    ("CHELABASE_APP_AUTH_INTERNAL_TOKEN", crate::mode_policy::DEV_APP_AUTH_INTERNAL_TOKEN),
    ("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", "chelabase-dev-app-sessions"),
];

/// `var`'s development default.
///
/// # Panics
/// When `var` isn't in [`DEV_BEARERS`] (a programming error, caught by every service's tests).
pub fn dev_bearer(var: &str) -> &'static str {
    DEV_BEARERS
        .iter()
        .find(|(v, _)| *v == var)
        .map(|(_, value)| *value)
        .unwrap_or_else(|| panic!("{var} has no development default"))
}

/// Whether the development defaults are allowed (self-hosted mode only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DevDefaults(bool);

impl DevDefaults {
    pub const ENV: &'static str = "CHELABASE_DEV_DEFAULTS";
    /// Only the workbench overlay sets this.
    pub const ON: Self = Self(true);
    pub const OFF: Self = Self(false);

    /// `1` is on; unset, blank or `0` is off; anything else is an error (a typo must not be
    /// read as either).
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("") | Some("0") => Ok(Self::OFF),
            Some("1") => Ok(Self::ON),
            Some(other) => Err(format!("{}={other:?}: expected 1 or 0", Self::ENV)),
        }
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        Self::parse(get(Self::ENV).as_deref())
    }

    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|n| std::env::var(n).ok())
    }

    pub fn is_on(self) -> bool {
        self.0
    }

    /// [`Self::check_bearer`] for each `(variable, token)`, against every value in
    /// [`DEV_BEARERS`] (one bearer set to another's default is just as public).
    pub fn check_bearers(self, policy: &ModePolicy, bearers: &[(&str, &str)]) -> Result<(), String> {
        let dev_values: Vec<&str> = DEV_BEARERS.iter().map(|(_, v)| *v).collect();
        for (var, token) in bearers {
            self.check_bearer(policy, var, token, &dev_values)?;
        }
        Ok(())
    }

    /// `Err` naming `var` when `token` is one of the public `dev_values` and neither the mode
    /// (platform never) nor this switch allows it. The message never repeats the token.
    pub fn check_bearer(
        self,
        policy: &ModePolicy,
        var: &str,
        token: &str,
        dev_values: &[&str],
    ) -> Result<(), String> {
        // Platform first: its own message, and no switch loosens it.
        policy.check_dev_bearer(var, token, dev_values)?;
        if !self.0 && dev_values.contains(&token) {
            return Err(format!(
                "{var} is unset or a development default, which is public: set it (or \
                 {var}_FILE) to a generated secret (`chelabase-init secrets` makes one). Only the \
                 workbench sets {}=1 to allow the development defaults",
                Self::ENV
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;

    const DEV: &str = "chelabase-dev-proxy-api";

    #[test]
    fn only_one_turns_it_on() {
        assert_eq!(DevDefaults::parse(None), Ok(DevDefaults::OFF));
        assert_eq!(DevDefaults::parse(Some(" ")), Ok(DevDefaults::OFF));
        assert_eq!(DevDefaults::parse(Some("0")), Ok(DevDefaults::OFF));
        assert_eq!(DevDefaults::parse(Some(" 1\n")), Ok(DevDefaults::ON));
        for typo in ["true", "yes", "on", "2"] {
            let err = DevDefaults::parse(Some(typo)).unwrap_err();
            assert!(err.contains("CHELABASE_DEV_DEFAULTS"), "{err}");
        }
    }

    // AC 4: self-hosted without the switch refuses a development bearer, naming the variable
    // and never the token; with it, today's behaviour; platform refuses either way.
    #[test]
    fn dev_bearer_refused_without_dev_defaults() {
        let sh = ModePolicy::new(Mode::SelfHosted);
        let pf = ModePolicy::new(Mode::Platform);
        let check = |d: DevDefaults, p: &ModePolicy, token: &str| {
            d.check_bearer(p, "CHELABASE_PROXY_API_TOKEN", token, &[DEV, "other-dev"])
        };
        let err = check(DevDefaults::OFF, &sh, DEV).unwrap_err();
        assert!(
            err.contains("CHELABASE_PROXY_API_TOKEN") && !err.contains(DEV),
            "{err}"
        );
        assert!(err.contains("CHELABASE_DEV_DEFAULTS"), "{err}");
        assert!(check(DevDefaults::OFF, &sh, "other-dev").is_err());
        assert_eq!(check(DevDefaults::ON, &sh, DEV), Ok(()));
        let err = check(DevDefaults::ON, &pf, DEV).unwrap_err();
        assert!(
            err.contains("CHELABASE_PROXY_API_TOKEN") && !err.contains(DEV),
            "{err}"
        );
        assert!(check(DevDefaults::OFF, &pf, DEV).is_err());
        for d in [DevDefaults::ON, DevDefaults::OFF] {
            for p in [&sh, &pf] {
                assert_eq!(check(d, p, "a-generated-secret"), Ok(()));
            }
        }
    }
}
