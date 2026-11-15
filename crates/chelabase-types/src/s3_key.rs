//! S3 access keys (s3-endpoint): an access key id `CBAK` + 16 characters of uppercase RFC 4648
//! base32 (20 characters, the AWS shape, with a prefix secret scanners can match), and a
//! 40-character `[A-Za-z0-9]` secret.
//!
//! SigV4 is an HMAC over the request, so the verifier needs the secret itself: the metadata
//! keeps it sealed (`chelabase_secrets::seal_s3_secret`), never a hash. Both halves come from
//! the OS CSPRNG without modulo bias: the id is ten random bytes in base32 (exactly 80 bits,
//! five bits a character), and each secret character is drawn by rejection sampling.

/// The access key id's prefix.
pub const ACCESS_KEY_ID_PREFIX: &str = "CBAK";
/// The access key id's length (prefix included).
pub const ACCESS_KEY_ID_LEN: usize = 20;
/// The secret's length.
pub const SECRET_LEN: usize = 40;

const ID_BODY_LEN: usize = ACCESS_KEY_ID_LEN - ACCESS_KEY_ID_PREFIX.len();
const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
const SECRET_ALPHABET: &[u8; 62] =
    b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A freshly generated pair: show `secret` once, seal it, never log it.
#[derive(Clone, PartialEq, Eq)]
pub struct S3KeyPair {
    pub access_key_id: String,
    pub secret: String,
}

impl std::fmt::Debug for S3KeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3KeyPair")
            .field("access_key_id", &self.access_key_id)
            .finish_non_exhaustive()
    }
}

/// A new access key id and secret from the OS random number generator.
pub fn generate() -> S3KeyPair {
    S3KeyPair {
        access_key_id: generate_access_key_id(),
        secret: generate_secret(),
    }
}

fn generate_access_key_id() -> String {
    let mut bytes = [0u8; ID_BODY_LEN * 5 / 8];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    let mut id = String::with_capacity(ACCESS_KEY_ID_LEN);
    id.push_str(ACCESS_KEY_ID_PREFIX);
    let (mut acc, mut bits) = (0u32, 0u32);
    for byte in bytes {
        acc = (acc << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            id.push(BASE32[((acc >> bits) & 31) as usize] as char);
        }
    }
    debug_assert_eq!((id.len(), bits), (ACCESS_KEY_ID_LEN, 0));
    id
}

fn generate_secret() -> String {
    // 248 = 4 * 62: bytes at or above it are dropped, so every character is equally likely.
    const LIMIT: u8 = (256 / SECRET_ALPHABET.len() * SECRET_ALPHABET.len()) as u8;
    let mut secret = String::with_capacity(SECRET_LEN);
    let mut buf = [0u8; 64];
    while secret.len() < SECRET_LEN {
        getrandom::fill(&mut buf).expect("OS random number generator");
        for &b in buf.iter().filter(|&&b| b < LIMIT) {
            if secret.len() == SECRET_LEN {
                break;
            }
            secret.push(SECRET_ALPHABET[usize::from(b) % SECRET_ALPHABET.len()] as char);
        }
    }
    secret
}

/// Whether `id` has the access key id's shape (`^CBAK[A-Z2-7]{16}$`): checked before any
/// lookup.
pub fn is_access_key_id(id: &str) -> bool {
    id.len() == ACCESS_KEY_ID_LEN
        && id.starts_with(ACCESS_KEY_ID_PREFIX)
        && id.as_bytes()[ACCESS_KEY_ID_PREFIX.len()..]
            .iter()
            .all(|b| BASE32.contains(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_shape(id: &str) -> bool {
        id.len() == 20
            && id.starts_with("CBAK")
            && id[4..]
                .bytes()
                .all(|b| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b))
    }

    #[test]
    fn generated_pairs_have_the_shape() {
        let mut seen_ids = std::collections::HashSet::new();
        let mut seen_secret_chars = std::collections::HashSet::new();
        let mut seen_id_chars = std::collections::HashSet::new();
        for _ in 0..500 {
            let pair = generate();
            assert!(id_shape(&pair.access_key_id), "{}", pair.access_key_id);
            assert!(
                is_access_key_id(&pair.access_key_id),
                "{}",
                pair.access_key_id
            );
            assert_eq!(pair.secret.len(), 40, "{}", pair.secret);
            assert!(
                pair.secret.bytes().all(|b| b.is_ascii_alphanumeric()),
                "{}",
                pair.secret
            );
            assert!(seen_ids.insert(pair.access_key_id.clone()), "a repeated id");
            seen_id_chars.extend(pair.access_key_id[4..].bytes());
            seen_secret_chars.extend(pair.secret.bytes());
        }
        // 500 pairs use the whole alphabets (missing one is ~1e-20 likely).
        assert_eq!(seen_id_chars.len(), 32);
        assert_eq!(seen_secret_chars.len(), 62);
        assert!(
            !format!("{:?}", generate()).contains("secret: "),
            "Debug hides the secret"
        );
    }

    #[test]
    fn is_access_key_id_refuses_bad_shapes() {
        let good = generate().access_key_id;
        assert!(is_access_key_id(&good));
        assert!(is_access_key_id("CBAKAAAAAAAAAAAAAAAA"));
        assert!(is_access_key_id("CBAK2345672345672345"));
        for bad in [
            "",
            "CBAK",
            &good[..19],
            &format!("{good}A"),
            &good.to_ascii_lowercase(),
            &good.replacen("CBAK", "cbak", 1),
            &good.replacen("CBAK", "AKIA", 1),
            "CBAKAAAAAAAAAAAAAAA1",
            "CBAKAAAAAAAAAAAAAAA8",
            "CBAKAAAAAAAAAAAAAAAa",
            "CBAKAAAAAAAAAAAAAAA=",
            "CBAKAAAAAAAAAAAAAA\u{e9}",
            " CBAKAAAAAAAAAAAAAAA",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            assert!(!is_access_key_id(bad), "{bad:?}");
        }
    }
}
