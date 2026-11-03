//! Every decision that differs between self-hosted and platform mode lives here, so no other
//! code compares a [`Mode`]. Platform mode fails closed.

use crate::mode::Mode;

/// The development bearer between `auth` and the orchestrator's internal listener
/// (`CHELABASE_APP_AUTH_INTERNAL_TOKEN` unset). It is in the repository, so it is public.
pub const DEV_APP_AUTH_INTERNAL_TOKEN: &str = "chelabase-dev-app-auth";

/// Whether a visitor may create their own account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignUpRule {
    /// Self-hosted: allowed or not per the tenant's own setting.
    GatedByTenant,
    /// Platform: self-sign-up is refused.
    Refused,
}

#[derive(Clone, Copy, Debug)]
pub struct ModePolicy {
    mode: Mode,
}

impl ModePolicy {
    /// `(name, description)` of every rule, in the order `describe` returns them (the docs table).
    pub const RULES: &'static [(&'static str, &'static str)] = &[
        ("instance_admin", "Who may use instance-level routes."),
        ("sign_up", "Whether a visitor may create their own account."),
        ("open_signup_switchable", "Whether the tenant may switch open sign-up on."),
        ("email_verification_default", "Whether sign-ups must verify their email by default."),
        ("owner_superuser_allowed", "Whether a database's owner role may be a superuser."),
        ("password_strength_forced", "Whether role passwords must pass the strength check."),
        ("prehashed_passwords_allowed", "Whether role passwords may be supplied pre-hashed."),
        ("ca_file_on_reveal", "Whether revealing connection details includes the dev CA path."),
        ("auto_create_members", "Whether a signed-in person with no member row becomes one on their first request."),
        ("default_internal_token_allowed", "Whether the services may start with the development internal bearer (a platform instance refuses to start with it)."),
    ];

    pub fn new(mode: Mode) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn self_hosted(&self) -> bool {
        matches!(self.mode, Mode::SelfHosted)
    }

    /// Who may use `Instance` routes: self-hosted, an active tenant admin; platform, only
    /// members of the operator organisation (nobody when it is unset).
    pub fn instance_admin(&self, active_tenant_admin: bool, in_operator_org: bool) -> bool {
        match self.mode {
            Mode::SelfHosted => active_tenant_admin,
            Mode::Platform => in_operator_org,
        }
    }

    /// Whether a visitor may create their own account: self-hosted, only while the tenant's open
    /// sign-up is on (or it has no member yet); platform, never, until its own design (fails
    /// closed).
    pub fn sign_up(&self) -> SignUpRule {
        match self.mode {
            Mode::SelfHosted => SignUpRule::GatedByTenant,
            Mode::Platform => SignUpRule::Refused,
        }
    }

    /// Whether a tenant admin may switch open sign-up on (self-hosted only).
    pub fn open_signup_switchable(&self) -> bool {
        self.self_hosted()
    }

    pub fn email_verification_default(&self) -> bool {
        !self.self_hosted()
    }

    pub fn owner_superuser_allowed(&self) -> bool {
        self.self_hosted()
    }

    pub fn password_strength_forced(&self) -> bool {
        !self.self_hosted()
    }

    pub fn ca_file_on_reveal(&self) -> bool {
        self.self_hosted()
    }

    pub fn prehashed_passwords_allowed(&self) -> bool {
        self.self_hosted()
    }

    /// Self-hosted: a signed-in person with no member row is admitted on their first
    /// request by the tenant's sign-up rule: the first one claims the admin, others become
    /// `pending` while open sign-up is on and are refused (no row) while it is off. Platform:
    /// members are only made explicitly.
    pub fn auto_create_members(&self) -> bool {
        self.self_hosted()
    }

    /// Self-hosted: the development internal bearer is fine (a local stack). Platform: it is a
    /// public secret, so a service that would use it refuses to start.
    pub fn default_internal_token_allowed(&self) -> bool {
        self.self_hosted()
    }

    /// `Err` (a message for the startup error) when `token` is the development bearer and this
    /// mode doesn't allow it. The message never repeats the token.
    pub fn check_internal_token(&self, token: &str) -> Result<(), String> {
        self.check_dev_bearer(
            "CHELABASE_APP_AUTH_INTERNAL_TOKEN",
            token,
            &[DEV_APP_AUTH_INTERNAL_TOKEN],
        )
    }

    /// `Err` naming `var` when `token` is one of the public development bearers `dev_values`
    /// and this mode doesn't allow them. The message never repeats the token.
    pub fn check_dev_bearer(&self, var: &str, token: &str, dev_values: &[&str]) -> Result<(), String> {
        if !self.default_internal_token_allowed() && dev_values.contains(&token) {
            return Err(format!(
                "{var} is unset or a development default, which is public: \
                 platform mode needs its own secret"
            ));
        }
        Ok(())
    }

    /// `(rule, this mode's value)` for every rule in [`Self::RULES`], in that order.
    pub fn describe(&self) -> Vec<(&'static str, String)> {
        Self::RULES
            .iter()
            .map(|&(name, _)| (name, self.value_of(name)))
            .collect()
    }

    fn value_of(&self, rule: &str) -> String {
        let b = |v: bool| v.to_string();
        match rule {
            "instance_admin" => match self.mode {
                Mode::SelfHosted => "active tenant admin",
                Mode::Platform => "member of the operator organisation",
            }
            .to_string(),
            "sign_up" => match self.sign_up() {
                SignUpRule::GatedByTenant => "gated by the tenant",
                SignUpRule::Refused => "refused",
            }
            .to_string(),
            "open_signup_switchable" => b(self.open_signup_switchable()),
            "email_verification_default" => b(self.email_verification_default()),
            "owner_superuser_allowed" => b(self.owner_superuser_allowed()),
            "password_strength_forced" => b(self.password_strength_forced()),
            "prehashed_passwords_allowed" => b(self.prehashed_passwords_allowed()),
            "ca_file_on_reveal" => b(self.ca_file_on_reveal()),
            "auto_create_members" => b(self.auto_create_members()),
            "default_internal_token_allowed" => b(self.default_internal_token_allowed()),
            other => unreachable!("rule {other} is in RULES but has no value"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_per_mode() {
        type Rule = fn(&ModePolicy) -> bool;
        let table: [(&str, Rule, bool, bool); 8] = [
            ("default_internal_token_allowed", ModePolicy::default_internal_token_allowed, true, false),
            ("open_signup_switchable", ModePolicy::open_signup_switchable, true, false),
            ("email_verification_default", ModePolicy::email_verification_default, false, true),
            ("owner_superuser_allowed", ModePolicy::owner_superuser_allowed, true, false),
            ("password_strength_forced", ModePolicy::password_strength_forced, false, true),
            ("prehashed_passwords_allowed", ModePolicy::prehashed_passwords_allowed, true, false),
            ("ca_file_on_reveal", ModePolicy::ca_file_on_reveal, true, false),
            ("auto_create_members", ModePolicy::auto_create_members, true, false),
        ];
        let sh = ModePolicy::new(Mode::SelfHosted);
        let pf = ModePolicy::new(Mode::Platform);
        assert_eq!(sh.mode(), Mode::SelfHosted);
        assert_eq!(pf.mode(), Mode::Platform);
        for (name, f, sh_value, pf_value) in table {
            assert_eq!(f(&sh), sh_value, "{name} self-hosted");
            assert_eq!(f(&pf), pf_value, "{name} platform");
            assert!(ModePolicy::RULES.iter().any(|r| r.0 == name), "{name} in RULES");
        }
        assert_eq!(sh.sign_up(), SignUpRule::GatedByTenant);
        assert_eq!(pf.sign_up(), SignUpRule::Refused);
        for (admin, operator) in [(false, false), (false, true), (true, false), (true, true)] {
            assert_eq!(sh.instance_admin(admin, operator), admin);
            assert_eq!(pf.instance_admin(admin, operator), operator);
        }
        // Reverse coverage: a rule added to RULES without a row here fails.
        let covered: Vec<&str> = table.iter().map(|r| r.0).chain(["instance_admin", "sign_up"]).collect();
        for (name, _) in ModePolicy::RULES {
            assert!(covered.contains(name), "RULES entry {name} has no row in this test");
        }
    }

    // M9: the development bearer between `auth` and the orchestrator's internal listener is
    // public; a platform instance refuses to start with it.
    #[test]
    fn platform_refuses_the_development_internal_token() {
        let sh = ModePolicy::new(Mode::SelfHosted);
        let pf = ModePolicy::new(Mode::Platform);
        assert_eq!(sh.check_internal_token(DEV_APP_AUTH_INTERNAL_TOKEN), Ok(()));
        assert!(pf.check_internal_token(DEV_APP_AUTH_INTERNAL_TOKEN).is_err());
        assert_eq!(pf.check_internal_token("a-long-random-secret"), Ok(()));
        assert_eq!(sh.check_internal_token("a-long-random-secret"), Ok(()));
        let message = pf.check_internal_token(DEV_APP_AUTH_INTERNAL_TOKEN).unwrap_err();
        assert!(message.contains("CHELABASE_APP_AUTH_INTERNAL_TOKEN") && !message.contains(DEV_APP_AUTH_INTERNAL_TOKEN), "{message}");
    }

    #[test]
    fn describe_lists_every_rule() {
        let names: Vec<_> = ModePolicy::RULES.iter().map(|r| r.0).collect();
        for (name, description) in ModePolicy::RULES {
            assert!(!description.trim().is_empty(), "{name} needs a description");
        }
        for mode in [Mode::SelfHosted, Mode::Platform] {
            let described: Vec<_> = ModePolicy::new(mode).describe().into_iter().map(|(n, _)| n).collect();
            assert_eq!(described, names);
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "no duplicate rule names");
    }

    #[test]
    fn platform_fails_closed() {
        let pf = ModePolicy::new(Mode::Platform);
        assert_eq!(pf.sign_up(), SignUpRule::Refused);
        assert!(!pf.instance_admin(true, false));
        assert!(!pf.instance_admin(false, false));
    }
}
