//! SCRAM-SHA-256 password verifiers in the format Postgres stores (RFC 5802 / RFC 7677).

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

const ITERATIONS: u32 = 4096;

/// `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`, with a random salt.
pub fn verifier(password: &str) -> String {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("OS random number generator");
    verifier_with_salt(password, &salt, ITERATIONS)
}

pub fn verifier_with_salt(password: &str, salt: &[u8], iterations: u32) -> String {
    // Like Postgres's pg_saslprep: normalize, and use the raw password if SASLprep rejects it.
    let prepared = stringprep::saslprep(password).unwrap_or(password.into());
    let mut salted = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<Sha256>(prepared.as_bytes(), salt, iterations, &mut salted);
    let client_key = hmac(&salted, b"Client Key");
    let server_key = hmac(&salted, b"Server Key");
    let stored_key = Sha256::digest(client_key);
    format!(
        "SCRAM-SHA-256${iterations}:{}${}:{}",
        STANDARD.encode(salt),
        STANDARD.encode(stored_key),
        STANDARD.encode(server_key)
    )
}

fn hmac(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key size");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test vector from RFC 7677 section 3 (user "user", password "pencil").
    #[test]
    fn matches_rfc7677_vector() {
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        let v = verifier_with_salt("pencil", &salt, 4096);
        let (_, keys) = v.rsplit_once('$').unwrap();
        let (stored, server) = keys.split_once(':').unwrap();
        // ServerKey from the RFC's server signature derivation; StoredKey = H(ClientKey).
        assert_eq!(server, "wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=");
        assert_eq!(stored, "WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=");
        assert!(v.starts_with("SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$"));
    }

    /// Postgres runs SASLprep on the password before hashing (`pg_saslprep`), so a
    /// password it normalizes must hash like its normalized form.
    #[test]
    fn saslprep_normalizes_the_password_first() {
        let salt = [7u8; 16];
        // U+00AA (feminine ordinal) is "a" under NFKC; U+00A0 (no-break space) maps to " ".
        assert_eq!(
            verifier_with_salt("p\u{00AA}ss\u{00A0}word", &salt, 4096),
            verifier_with_salt("pass word", &salt, 4096)
        );
    }

    /// When SASLprep rejects a password (here: a control character), Postgres hashes the
    /// raw bytes instead of failing.
    #[test]
    fn a_password_saslprep_rejects_is_hashed_raw() {
        let salt = [7u8; 16];
        let raw = "bell\u{0007}";
        assert!(stringprep::saslprep(raw).is_err());
        assert_ne!(
            verifier_with_salt(raw, &salt, 4096),
            verifier_with_salt("bell", &salt, 4096)
        );
        assert!(verifier_with_salt(raw, &salt, 4096).starts_with("SCRAM-SHA-256$4096:"));
    }

    #[test]
    fn each_verifier_gets_a_fresh_16_byte_salt() {
        let (a, b) = (verifier("pw"), verifier("pw"));
        assert_ne!(a, b);
        assert!(a.starts_with("SCRAM-SHA-256$4096:"));
        let salt = a.split('$').nth(1).unwrap().split_once(':').unwrap().1;
        assert_eq!(STANDARD.decode(salt).unwrap().len(), 16);
    }
}
