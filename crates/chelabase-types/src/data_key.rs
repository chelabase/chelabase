//! Data API keys: `chk_pub_` / `chk_sec_` followed by 32 random bytes in base62.
//!
//! The key is shown once; only its SHA-256 (of the whole string) is stored, and the first
//! twelve characters are kept as a display prefix.

use serde::{Deserialize, Serialize};

/// Roles a secret key's role may not be a member of, at any depth (besides any superuser
/// role): owner level (`neon_superuser`) and roles that reach the compute itself or
/// Chelabase's machinery. The worker's create-time check and rest's per-request gate both
/// refuse exactly these (with superuser, CREATEROLE, CREATEDB and REPLICATION; E33).
pub const KEY_ROLE_REFUSED_GROUPS: [&str; 5] = [
    "neon_superuser",
    "chelabase_admin",
    "pg_execute_server_program",
    "pg_read_server_files",
    "pg_write_server_files",
];

const PUBLIC_PREFIX: &str = "chk_pub_";
const SECRET_PREFIX: &str = "chk_sec_";
const BODY_LEN: usize = 43;
const KEY_LEN: usize = 51;
const DISPLAY_PREFIX_LEN: usize = 12;
const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum DataKeyKind {
    Public,
    Secret,
}

impl DataKeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Secret => "secret",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "public" => Some(Self::Public),
            "secret" => Some(Self::Secret),
            _ => None,
        }
    }

    fn key_prefix(self) -> &'static str {
        match self {
            Self::Public => PUBLIC_PREFIX,
            Self::Secret => SECRET_PREFIX,
        }
    }
}

/// A freshly generated key: show `key` once, store `prefix` and `sha256`.
#[derive(Clone, PartialEq, Eq)]
pub struct GeneratedKey {
    pub key: String,
    pub prefix: String,
    pub sha256: [u8; 32],
}

impl std::fmt::Debug for GeneratedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeneratedKey")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

pub fn generate(kind: DataKeyKind) -> GeneratedKey {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    let key = format!("{}{}", kind.key_prefix(), base62(bytes));
    debug_assert_eq!(key.len(), KEY_LEN);
    GeneratedKey {
        prefix: key[..DISPLAY_PREFIX_LEN].to_string(),
        sha256: hash(&key),
        key,
    }
}

pub fn hash(key: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(key.as_bytes()).into()
}

/// The kind of a well-formed key; `None` unless length, prefix and alphabet all match.
pub fn parse_kind(key: &str) -> Option<DataKeyKind> {
    if key.len() != KEY_LEN {
        return None;
    }
    let kind = [DataKeyKind::Public, DataKeyKind::Secret]
        .into_iter()
        .find(|k| key.starts_with(k.key_prefix()))?;
    key.as_bytes()[KEY_LEN - BODY_LEN..]
        .iter()
        .all(|b| ALPHABET.contains(b))
        .then_some(kind)
}

/// `bytes` as a big-endian base62 number, left-padded with `0` to [`BODY_LEN`] characters.
fn base62(bytes: [u8; 32]) -> String {
    let mut number = bytes.to_vec();
    let mut digits = Vec::with_capacity(BODY_LEN);
    while number.iter().any(|&b| b != 0) {
        let mut remainder = 0u32;
        for byte in number.iter_mut() {
            let acc = (remainder << 8) | u32::from(*byte);
            *byte = (acc / 62) as u8;
            remainder = acc % 62;
        }
        digits.push(ALPHABET[remainder as usize]);
    }
    digits.resize(BODY_LEN, b'0');
    digits.reverse();
    String::from_utf8(digits).expect("ASCII alphabet")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body_ok(key: &str) -> bool {
        key[8..].bytes().all(|b| ALPHABET.contains(&b))
    }

    #[test]
    fn generated_keys_have_the_exact_format() {
        for (kind, prefix) in [
            (DataKeyKind::Public, "chk_pub_"),
            (DataKeyKind::Secret, "chk_sec_"),
        ] {
            for _ in 0..50 {
                let g = generate(kind);
                assert_eq!(g.key.len(), 51);
                assert!(g.key.starts_with(prefix));
                assert!(body_ok(&g.key), "{}", g.key);
                assert_eq!(g.prefix, g.key[..12]);
                assert_eq!(g.sha256, hash(&g.key));
                assert_eq!(parse_kind(&g.key), Some(kind));
            }
        }
    }

    #[test]
    fn parse_kind_rejects_wrong_length_prefix_and_alphabet() {
        let good = generate(DataKeyKind::Public).key;
        assert_eq!(parse_kind(&good), Some(DataKeyKind::Public));
        assert_eq!(parse_kind(&good[..50]), None);
        assert_eq!(parse_kind(&format!("{good}0")), None);
        assert_eq!(parse_kind(""), None);
        assert_eq!(parse_kind(&good.replacen("chk_pub_", "chk_xyz_", 1)), None);
        assert_eq!(parse_kind(&good.replacen("chk_pub_", "CHK_PUB_", 1)), None);
        let mut bad = good.clone();
        bad.replace_range(20..21, "-");
        assert_eq!(parse_kind(&bad), None);
        let mut bad = good.clone();
        bad.replace_range(20..21, "é");
        assert_eq!(parse_kind(&bad), None);
        assert_eq!(parse_kind(&format!("{good} ")[1..]), None);
    }

    #[test]
    fn hash_is_sha256_of_the_whole_string() {
        // sha256("abc")
        assert_eq!(
            hash("abc")
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn two_generated_keys_differ() {
        let (a, b) = (generate(DataKeyKind::Secret), generate(DataKeyKind::Secret));
        assert_ne!(a.key, b.key);
        assert_ne!(a.sha256, b.sha256);
    }

    #[test]
    fn kinds_have_names() {
        assert_eq!(DataKeyKind::Public.as_str(), "public");
        assert_eq!(DataKeyKind::Secret.as_str(), "secret");
        assert_eq!(DataKeyKind::parse("secret"), Some(DataKeyKind::Secret));
        assert_eq!(DataKeyKind::parse("x"), None);
    }
}
