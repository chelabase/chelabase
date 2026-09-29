//! Encryption at rest for control-plane secrets (database passwords).
//!
//! AES-256-GCM with a random 96-bit nonce. The database id is authenticated
//! associated data, so a ciphertext only opens for the row it was made for.
//! Stored as `v1:` + base64(nonce ‖ ciphertext). Database credentials bind
//! their purpose (owner vs platform) too, so the two can't be swapped.

use aes_gcm::aead::{Aead, Generate, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::path::Path;
use thiserror::Error;

const PREFIX: &str = "v1:";
const NONCE_LEN: usize = 12;

#[derive(Debug, Error)]
pub enum SecretsError {
    #[error("secrets key: {0}")]
    Config(String),
    #[error("malformed sealed value")]
    Malformed,
    #[error("could not decrypt sealed value (wrong key or row)")]
    Decrypt,
}

#[derive(Clone)]
pub struct SecretKey(Key<Aes256Gcm>);

impl SecretKey {
    pub fn generate() -> Self {
        Self(Key::<Aes256Gcm>::generate())
    }

    pub fn to_base64(&self) -> String {
        STANDARD.encode(self.0.as_slice())
    }

    pub fn from_base64(encoded: &str) -> Result<Self, SecretsError> {
        let bytes = STANDARD
            .decode(encoded.trim())
            .map_err(|e| SecretsError::Config(format!("not base64: {e}")))?;
        if bytes.len() != 32 {
            return Err(SecretsError::Config(format!(
                "expected 32 bytes, got {}",
                bytes.len()
            )));
        }
        let key = Key::<Aes256Gcm>::try_from(bytes.as_slice())
            .map_err(|_| SecretsError::Config("expected 32 bytes".into()))?;
        Ok(Self(key))
    }

    pub fn from_file(path: &Path) -> Result<Self, SecretsError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SecretsError::Config(format!("read {}: {e}", path.display())))?;
        Self::from_base64(&text)
    }

    /// From the file named by `CHELADB_SECRETS_KEY`.
    pub fn from_env() -> Result<Self, SecretsError> {
        let path = std::env::var("CHELADB_SECRETS_KEY").map_err(|_| {
            SecretsError::Config(
                "CHELADB_SECRETS_KEY is not set; run `just zitadel-bootstrap` and load .cheladb/zitadel/demo.env"
                    .into(),
            )
        })?;
        Self::from_file(Path::new(&path))
    }
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretKey(..)")
    }
}

/// Associated data for the key-check value (never a database id).
const KEY_CHECK_AAD: &str = "cheladb:secrets-key-check";
const KEY_CHECK_PLAINTEXT: &str = "cheladb secrets key check";

/// A value only `key` can open; stored so services can tell, at startup,
/// whether they hold the key existing credentials were encrypted with.
pub fn key_check_value(key: &SecretKey) -> String {
    seal(key, KEY_CHECK_AAD, KEY_CHECK_PLAINTEXT)
}

pub fn verify_key_check(key: &SecretKey, stored: &str) -> bool {
    open(key, KEY_CHECK_AAD, stored).is_ok_and(|p| p == KEY_CHECK_PLAINTEXT)
}

/// Which credential of a database a sealed value holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    Owner,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Purpose::Owner => "owner",
        }
    }
}

/// Encrypt `plaintext`, authenticated with `aad` (it only opens with the same `aad`).
pub fn seal(key: &SecretKey, aad: &str, plaintext: &str) -> String {
    seal_with(PREFIX, key, aad, plaintext)
}

/// Decrypt a value produced by [`seal`] for the same `aad`.
pub fn open(key: &SecretKey, aad: &str, sealed: &str) -> Result<String, SecretsError> {
    open_with(PREFIX, key, aad, sealed)
}

/// Encrypt a database credential, bound to both its database and its purpose.
pub fn seal_credential(key: &SecretKey, database_id: &str, purpose: Purpose, plaintext: &str) -> String {
    seal(key, &credential_aad(database_id, purpose), plaintext)
}

/// Decrypt a credential sealed by [`seal_credential`] for the same database and purpose.
pub fn open_credential(
    key: &SecretKey,
    database_id: &str,
    purpose: Purpose,
    sealed: &str,
) -> Result<String, SecretsError> {
    open(key, &credential_aad(database_id, purpose), sealed)
}

fn credential_aad(database_id: &str, purpose: Purpose) -> String {
    format!("{database_id}:{}", purpose.as_str())
}

fn seal_with(prefix: &str, key: &SecretKey, aad: &str, plaintext: &str) -> String {
    let nonce = Nonce::generate();
    let ciphertext = Aes256Gcm::new(&key.0)
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .expect("AES-GCM encryption cannot fail for in-memory input");
    let mut bytes = nonce.to_vec();
    bytes.extend_from_slice(&ciphertext);
    format!("{prefix}{}", STANDARD.encode(bytes))
}

fn open_with(prefix: &str, key: &SecretKey, aad: &str, sealed: &str) -> Result<String, SecretsError> {
    let encoded = sealed.strip_prefix(prefix).ok_or(SecretsError::Malformed)?;
    let bytes = STANDARD.decode(encoded).map_err(|_| SecretsError::Malformed)?;
    if bytes.len() <= NONCE_LEN {
        return Err(SecretsError::Malformed);
    }
    let (nonce, ciphertext) = bytes.split_at(NONCE_LEN);
    let plaintext = Aes256Gcm::new(&key.0)
        .decrypt(
            &Nonce::try_from(nonce).map_err(|_| SecretsError::Malformed)?,
            Payload {
                msg: ciphertext,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| SecretsError::Decrypt)?;
    String::from_utf8(plaintext).map_err(|_| SecretsError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_for_the_same_database() {
        let key = SecretKey::generate();
        let sealed = seal(&key, "db-1", "hunter2hunter2");
        assert!(sealed.starts_with("v1:"));
        assert_eq!(open(&key, "db-1", &sealed).unwrap(), "hunter2hunter2");
    }

    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        let key = SecretKey::generate();
        assert_ne!(seal(&key, "db-1", "same"), seal(&key, "db-1", "same"));
    }

    #[test]
    fn a_sealed_value_only_opens_for_its_own_database() {
        let key = SecretKey::generate();
        let sealed = seal(&key, "db-1", "pw");
        assert!(matches!(open(&key, "db-2", &sealed), Err(SecretsError::Decrypt)));
    }

    #[test]
    fn the_wrong_key_is_rejected() {
        let sealed = seal(&SecretKey::generate(), "db-1", "pw");
        assert!(matches!(
            open(&SecretKey::generate(), "db-1", &sealed),
            Err(SecretsError::Decrypt)
        ));
    }

    #[test]
    fn tampering_is_rejected() {
        let key = SecretKey::generate();
        let mut sealed = seal(&key, "db-1", "pw");
        let last = sealed.pop().unwrap();
        sealed.push(if last == 'A' { 'B' } else { 'A' });
        assert!(open(&key, "db-1", &sealed).is_err());
    }

    #[test]
    fn malformed_values_are_rejected() {
        let key = SecretKey::generate();
        for bad in ["", "v2:abc", "v1:not-base64!", "v1:AAAA"] {
            assert!(
                matches!(open(&key, "db-1", bad), Err(SecretsError::Malformed)),
                "{bad}"
            );
        }
    }

    #[test]
    fn keys_survive_base64_and_files() {
        let key = SecretKey::generate();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("k");
        std::fs::write(&path, key.to_base64()).unwrap();
        let loaded = SecretKey::from_file(&path).unwrap();
        let sealed = seal(&key, "db-1", "pw");
        assert_eq!(open(&loaded, "db-1", &sealed).unwrap(), "pw");
        assert!(matches!(
            SecretKey::from_base64("short"),
            Err(SecretsError::Config(_))
        ));
    }
}

#[cfg(test)]
mod key_check_tests {
    use super::*;

    #[test]
    fn key_check_verifies_only_with_the_same_key() {
        let key = SecretKey::generate();
        let check = key_check_value(&key);
        assert!(verify_key_check(&key, &check));
        assert!(!verify_key_check(&SecretKey::generate(), &check));
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn credentials_round_trip_for_their_database_and_purpose() {
        let key = SecretKey::generate();
        let sealed = seal_credential(&key, "db-1", Purpose::Owner, "pw");
        assert!(sealed.starts_with("v1:"));
        assert_eq!(open_credential(&key, "db-1", Purpose::Owner, &sealed).unwrap(), "pw");
    }

    // One format only: a value bound to the database id alone doesn't open as a credential.
    #[test]
    fn values_without_a_purpose_do_not_open_as_credentials() {
        let key = SecretKey::generate();
        let unbound = seal(&key, "db-1", "pw");
        assert!(open_credential(&key, "db-1", Purpose::Owner, &unbound).is_err());
    }
}
