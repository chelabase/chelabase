//! Password rules for roles forwarded from SQL (spec 2026-10-01-login-roles, rev 2).

use crate::role_settings::RoleSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordKind {
    Scram,
    Md5,
    Other,
}

/// `SCRAM-SHA-256$` prefix is SCRAM; `md5` followed by 32 hex digits is MD5.
pub fn hash_kind(s: &str) -> PasswordKind {
    if s.starts_with("SCRAM-SHA-256$") {
        PasswordKind::Scram
    } else if s.len() == 35 && s.starts_with("md5") && s[3..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        PasswordKind::Md5
    } else {
        PasswordKind::Other
    }
}

/// Estimated entropy: length times log2 of the pool of character classes used
/// (lowercase 26, uppercase 26, digits 10, anything else 33).
pub fn entropy_bits(password: &str) -> f64 {
    let (mut lower, mut upper, mut digit, mut other) = (false, false, false, false);
    let mut len = 0usize;
    for c in password.chars() {
        len += 1;
        match c {
            'a'..='z' => lower = true,
            'A'..='Z' => upper = true,
            '0'..='9' => digit = true,
            _ => other = true,
        }
    }
    let pool = 26 * lower as u32 + 26 * upper as u32 + 10 * digit as u32 + 33 * other as u32;
    if pool == 0 {
        return 0.0;
    }
    len as f64 * (pool as f64).log2()
}

#[derive(Debug, PartialEq, Eq)]
pub enum PasswordRefusal {
    Md5,
    NotScram,
    Prehashed,
    Weak,
}

impl PasswordRefusal {
    pub fn message(&self) -> &'static str {
        match self {
            Self::Md5 => "MD5 passwords can't be used through Cheladb; set a plain password",
            Self::NotScram => {
                "this password format can't be used through Cheladb; set a plain password"
            }
            Self::Prehashed => "pre-hashed passwords aren't accepted here; set a plain password",
            Self::Weak => {
                "password too weak: use at least 12 characters and mix letters, digits and symbols (about 60 bits)"
            }
        }
    }
}

/// The receiver's check of one forwarded `set` with a password.
pub fn check_forwarded_password(
    password: &str,
    encrypted: &str,
    rules: &RoleSettings,
) -> Result<(), PasswordRefusal> {
    match hash_kind(encrypted) {
        PasswordKind::Md5 => return Err(PasswordRefusal::Md5),
        PasswordKind::Other => return Err(PasswordRefusal::NotScram),
        PasswordKind::Scram => {}
    }
    if hash_kind(password) != PasswordKind::Other {
        // Strength applies to plain passwords only.
        return if rules.prehashed_allowed {
            Ok(())
        } else {
            Err(PasswordRefusal::Prehashed)
        };
    }
    if rules.strength && (password.chars().count() < 12 || entropy_bits(password) < 60.0) {
        return Err(PasswordRefusal::Weak);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mode::Mode;

    const SCRAM: &str = "SCRAM-SHA-256$4096:c2FsdHNhbHRzYWx0c2FsdA==$c3RvcmVka2V5c3RvcmVka2V5c3RvcmVka2V5c3RvcmU=:c2VydmVya2V5c2VydmVya2V5c2VydmVya2V5c2VydmVy";
    const MD5: &str = "md5aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn rules(mode: Mode, strength: bool) -> RoleSettings {
        RoleSettings::from_lookup(mode, |k| {
            (k == "CHELADB_PASSWORD_STRENGTH").then(|| if strength { "on" } else { "off" }.into())
        })
        .unwrap()
    }

    #[test]
    fn hash_kinds() {
        assert_eq!(hash_kind(SCRAM), PasswordKind::Scram);
        assert_eq!(hash_kind(MD5), PasswordKind::Md5);
        assert_eq!(hash_kind("md5short"), PasswordKind::Other);
        assert_eq!(
            hash_kind("md5zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"),
            PasswordKind::Other
        );
        assert_eq!(hash_kind("hunter2"), PasswordKind::Other);
    }

    #[test]
    fn entropy_examples() {
        let e12 = entropy_bits("abcdefghijkl");
        assert!((e12 - 56.4).abs() < 0.1, "{e12}");
        assert!(entropy_bits("abcdefghijklm") >= 60.0);
        assert!(entropy_bits("Tr0ub4dor&3x") >= 60.0);
        assert_eq!(entropy_bits(""), 0.0);
    }

    #[test]
    fn forwarded_password_table() {
        use PasswordRefusal::*;
        let strong = "Tr0ub4dor&3x";
        type Case = (
            &'static str,
            &'static str,
            Mode,
            bool,
            Result<(), PasswordRefusal>,
        );
        let cases: Vec<Case> = vec![
            (strong, MD5, Mode::SelfHosted, false, Err(Md5)),
            (strong, MD5, Mode::Platform, true, Err(Md5)),
            (strong, "plain", Mode::SelfHosted, false, Err(NotScram)),
            (SCRAM, SCRAM, Mode::SelfHosted, false, Ok(())),
            (SCRAM, SCRAM, Mode::SelfHosted, true, Ok(())),
            (SCRAM, SCRAM, Mode::Platform, false, Err(Prehashed)),
            (MD5, SCRAM, Mode::SelfHosted, false, Ok(())),
            (strong, SCRAM, Mode::SelfHosted, true, Ok(())),
            (strong, SCRAM, Mode::Platform, false, Ok(())),
            ("abcdefghijkl", SCRAM, Mode::SelfHosted, true, Err(Weak)),
            ("abcdefghijkl", SCRAM, Mode::SelfHosted, false, Ok(())),
            ("abcdefghijkl", SCRAM, Mode::Platform, false, Err(Weak)),
            ("Tr0ub&3xYz!", SCRAM, Mode::SelfHosted, true, Err(Weak)),
            ("Tr0ub&3xYz!", SCRAM, Mode::SelfHosted, false, Ok(())),
        ];
        for (pw, enc, mode, strength, want) in cases {
            let got = check_forwarded_password(pw, enc, &rules(mode, strength));
            assert_eq!(got, want, "{pw} {enc:.12} {mode:?} {strength}");
        }
    }

    #[test]
    fn refusal_messages_are_the_approved_texts() {
        assert_eq!(
            PasswordRefusal::Md5.message(),
            "MD5 passwords can't be used through Cheladb; set a plain password"
        );
        assert_eq!(
            PasswordRefusal::NotScram.message(),
            "this password format can't be used through Cheladb; set a plain password"
        );
        assert_eq!(
            PasswordRefusal::Prehashed.message(),
            "pre-hashed passwords aren't accepted here; set a plain password"
        );
        assert_eq!(
            PasswordRefusal::Weak.message(),
            "password too weak: use at least 12 characters and mix letters, digits and symbols (about 60 bits)"
        );
    }
}
