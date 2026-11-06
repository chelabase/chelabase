//! Input rules shared by every caller of the identity service.

/// The ban reason of a person removed from their last tenant. Only this ban is ours to lift
/// (reinstate here, accept-invite in `services/auth`); any other reason is left in place.
pub const MEMBER_REMOVED_BAN_REASON: &str = "chelabase:removed-from-last-tenant";

/// The complexity rule checked before a password reaches the service, so the answer never
/// depends on whether an account exists.
pub fn strong_password(password: &str) -> bool {
    password.chars().count() >= 8
        && password.chars().any(|c| c.is_lowercase())
        && password.chars().any(|c| c.is_uppercase())
        && password.chars().any(|c| c.is_ascii_digit())
        && password.chars().any(|c| !c.is_alphanumeric())
}

/// The longest password taken, in bytes; a longer one is refused as weak, before any account is
/// looked up.
pub const PASSWORD_MAX_BYTES: usize = 72;

pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}
