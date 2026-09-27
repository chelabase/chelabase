//! Database passwords.

/// 32 alphanumeric characters: strong, and safe inside SQL literals and URIs.
pub fn generate_password() -> String {
    use rand::{distributions::Alphanumeric, Rng};
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_32_alphanumerics_and_differ() {
        let (a, b) = (generate_password(), generate_password());
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(a, b);
    }
}
