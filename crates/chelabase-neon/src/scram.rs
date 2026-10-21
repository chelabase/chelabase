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

/// Whether `stored` (a `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>` verifier,
/// as `pg_authid.rolpassword` holds it) is `password`'s: the password is hashed again with the
/// verifier's own salt and iteration count. Anything else never matches.
pub fn verifies(stored: &str, password: &str) -> bool {
    let Some(rest) = stored.strip_prefix("SCRAM-SHA-256$") else {
        return false;
    };
    let Some((params, _keys)) = rest.split_once('$') else {
        return false;
    };
    let Some((iterations, salt)) = params.split_once(':') else {
        return false;
    };
    let (Ok(iterations), Ok(salt)) = (iterations.parse::<u32>(), STANDARD.decode(salt)) else {
        return false;
    };
    if iterations == 0 || salt.is_empty() {
        return false;
    }
    verifier_with_salt(password, &salt, iterations) == stored
}

/// Whether `s` is a whole SCRAM-SHA-256 verifier as Postgres' `parse_scram_secret` reads one:
/// `SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`, the iterations a positive
/// decimal integer (digits only), the salt non-empty standard base64, both keys standard
/// base64 of exactly 32 bytes, and nothing else (no extra parts, no whitespace). A string that
/// isn't one would be taken by `CREATE ROLE … PASSWORD '…'` as a plaintext password.
pub fn is_verifier(s: &str) -> bool {
    const KEY_LEN: usize = 32;
    let Some(rest) = s.strip_prefix("SCRAM-SHA-256$") else {
        return false;
    };
    let mut parts = rest.split('$');
    let (Some(params), Some(keys), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let mut params = params.split(':');
    let (Some(iterations), Some(salt), None) = (params.next(), params.next(), params.next())
    else {
        return false;
    };
    let mut keys = keys.split(':');
    let (Some(stored), Some(server), None) = (keys.next(), keys.next(), keys.next()) else {
        return false;
    };
    let iterations_ok = !iterations.is_empty()
        && iterations.bytes().all(|b| b.is_ascii_digit())
        && iterations.parse::<i32>().is_ok_and(|n| n > 0);
    let salt_ok = STANDARD.decode(salt).is_ok_and(|salt| !salt.is_empty());
    let key_ok = |k: &str| STANDARD.decode(k).is_ok_and(|k| k.len() == KEY_LEN);
    iterations_ok && salt_ok && key_ok(stored) && key_ok(server)
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

    /// A stored verifier is checked against a password with its own salt and iteration count;
    /// anything that isn't a SCRAM-SHA-256 verifier never matches.
    #[test]
    fn a_verifier_matches_only_its_password() {
        let v = verifier("pw");
        assert!(verifies(&v, "pw"));
        assert!(!verifies(&v, "pw2"));
        let salt = STANDARD.decode("W22ZaJ0SNY7soEsUEjb6gQ==").unwrap();
        assert!(verifies(&verifier_with_salt("pencil", &salt, 1000), "pencil"));
        for bad in ["", "md5abc", "SCRAM-SHA-256$x", "SCRAM-SHA-256$4096:!!$a:b", "SCRAM-SHA-256$0:AA==$a:b"] {
            assert!(!verifies(bad, "pw"), "{bad}");
        }
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
