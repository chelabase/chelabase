//! Input rules shared by every caller of Zitadel.

/// Zitadel's default complexity policy, checked here first so the answer never depends on
/// whether an account exists.
pub fn strong_password(password: &str) -> bool {
    password.chars().count() >= 8
        && password.chars().any(|c| c.is_lowercase())
        && password.chars().any(|c| c.is_uppercase())
        && password.chars().any(|c| c.is_ascii_digit())
        && password.chars().any(|c| !c.is_alphanumeric())
}

pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}
