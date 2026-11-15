//! Encryption at rest for control-plane secrets (database passwords).
//!
//! AES-256-GCM with a random 96-bit nonce. The database id is authenticated
//! associated data, so a ciphertext only opens for the row it was made for.
//! Stored as `v1:` + base64(nonce ‖ ciphertext). Database credentials bind
//! their purpose (owner vs authenticator) too, so the two can't be swapped.

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

    /// From the file named by `CHELABASE_SECRETS_KEY`.
    pub fn from_env() -> Result<Self, SecretsError> {
        let path = std::env::var("CHELABASE_SECRETS_KEY").map_err(|_| {
            SecretsError::Config(
                "CHELABASE_SECRETS_KEY is not set; a workbench stack sets it in its stack.env (`just workbench env` prints the path)"
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
const KEY_CHECK_AAD: &str = "chelabase:secrets-key-check";
const KEY_CHECK_PLAINTEXT: &str = "chelabase secrets key check";

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
    /// The `authenticator` role's password on a branch.
    Authenticator,
    /// A public data API key, sealed so it can be shown again.
    DataKey,
    /// A login role's password (bound to its branch and name, see [`seal_role_password`]).
    Role,
    /// A member's hidden login role password (`cb_m_…`, bound to its branch and name, see
    /// [`seal_member_role_password`]).
    MemberRole,
    /// The `chelabase_storage` login's password on a branch (the storage service's own).
    Storage,
    /// The `chelabase_functions_dispatch` login's password on a branch (the functions
    /// service's database-event dispatcher, edge functions m13b).
    FunctionsDispatch,
    /// The key storage signs its object URLs with ([`derive_key`]; never a sealed value).
    StorageUrl,
    /// A storage webhook endpoint's signing secret (bound to the endpoint, see
    /// [`seal_webhook_secret`]).
    Webhook,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::Owner => "owner",
            Purpose::Authenticator => "authenticator",
            Purpose::DataKey => "data_key",
            Purpose::Role => "role",
            Purpose::MemberRole => "member_role",
            Purpose::Storage => "storage",
            Purpose::FunctionsDispatch => "functions_dispatch",
            Purpose::StorageUrl => "storage_url",
            Purpose::Webhook => "webhook",
        }
    }
}

/// A 32-byte key for one `purpose` and generation (`kid`), derived from `key`:
/// HMAC-SHA256 over a fixed label, so no two purposes or generations share a key and the
/// derived key reveals nothing of `key`.
pub fn derive_key(key: &SecretKey, purpose: Purpose, kid: u32) -> [u8; 32] {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(key.0.as_slice())
        .expect("HMAC accepts any key size");
    mac.update(format!("chelabase:derive:{}:{kid}", purpose.as_str()).as_bytes());
    mac.finalize().into_bytes().into()
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

/// Seal a login role's password; the AAD is `{branch_id}:role:{role}`.
pub fn seal_role_password(key: &SecretKey, branch_id: &str, role: &str, password: &str) -> String {
    seal(key, &role_aad(branch_id, role), password)
}

/// Open a value from [`seal_role_password`] for the same branch and role.
pub fn open_role_password(
    key: &SecretKey,
    branch_id: &str,
    role: &str,
    sealed: &str,
) -> Result<String, SecretsError> {
    open(key, &role_aad(branch_id, role), sealed)
}

/// Seal a member role's password; the AAD is `{branch_id}:member_role:{role}`, so it opens
/// only for that role on that branch (never on a child branch that inherited the role).
pub fn seal_member_role_password(
    key: &SecretKey,
    branch_id: uuid::Uuid,
    role: &str,
    password: &str,
) -> Result<String, SecretsError> {
    Ok(seal(key, &member_role_aad(branch_id, role), password))
}

/// Open a value from [`seal_member_role_password`] for the same branch and role.
pub fn open_member_role_password(
    key: &SecretKey,
    branch_id: uuid::Uuid,
    role: &str,
    sealed: &str,
) -> Result<String, SecretsError> {
    open(key, &member_role_aad(branch_id, role), sealed)
}

/// Seal a storage webhook endpoint's signing secret; the AAD is `{endpoint_id}:webhook`, so it
/// opens only for that endpoint.
pub fn seal_webhook_secret(key: &SecretKey, endpoint_id: uuid::Uuid, secret: &str) -> String {
    seal_credential(key, &endpoint_id.to_string(), Purpose::Webhook, secret)
}

/// Open a value from [`seal_webhook_secret`] for the same endpoint.
pub fn open_webhook_secret(
    key: &SecretKey,
    endpoint_id: uuid::Uuid,
    sealed: &str,
) -> Result<String, SecretsError> {
    open_credential(key, &endpoint_id.to_string(), Purpose::Webhook, sealed)
}

fn member_role_aad(branch_id: uuid::Uuid, role: &str) -> String {
    format!("{branch_id}:{}:{role}", Purpose::MemberRole.as_str())
}

fn role_aad(branch_id: &str, role: &str) -> String {
    format!("{branch_id}:{}:{role}", Purpose::Role.as_str())
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
mod derive_tests {
    use super::*;

    #[test]
    fn derived_keys_are_stable_and_bound_to_key_purpose_and_generation() {
        let key = SecretKey::from_base64(&STANDARD.encode([7u8; 32])).unwrap();
        let a = derive_key(&key, Purpose::StorageUrl, 1);
        assert_eq!(a, derive_key(&key, Purpose::StorageUrl, 1));
        assert_ne!(a, derive_key(&key, Purpose::StorageUrl, 2));
        assert_ne!(a, derive_key(&key, Purpose::Storage, 1));
        assert_ne!(a, derive_key(&SecretKey::generate(), Purpose::StorageUrl, 1));
        assert_ne!(&a[..], &[7u8; 32][..]);
        assert_eq!(Purpose::StorageUrl.as_str(), "storage_url");
    }
}

#[cfg(test)]
mod webhook_tests {
    use super::*;

    // m18: a storage webhook's signing secret opens only for its own endpoint, and never as
    // another purpose's credential (nor the reverse).
    #[test]
    fn webhook_secrets_bind_their_endpoint_and_purpose() {
        let key = SecretKey::generate();
        assert_eq!(Purpose::Webhook.as_str(), "webhook");
        let (endpoint, other) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let sealed = seal_webhook_secret(&key, endpoint, "whsec_x");
        assert_eq!(open_webhook_secret(&key, endpoint, &sealed).unwrap(), "whsec_x");
        assert!(matches!(open_webhook_secret(&key, other, &sealed), Err(SecretsError::Decrypt)));
        assert_eq!(open(&key, &format!("{endpoint}:webhook"), &sealed).unwrap(), "whsec_x");
        for purpose in [Purpose::Owner, Purpose::DataKey, Purpose::Storage] {
            let as_other = seal_credential(&key, &endpoint.to_string(), purpose, "whsec_x");
            assert!(open_webhook_secret(&key, endpoint, &as_other).is_err(), "{purpose:?}");
            assert!(open_credential(&key, &endpoint.to_string(), purpose, &sealed).is_err(), "{purpose:?}");
        }
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

    #[test]
    fn authenticator_and_data_key_purposes_round_trip_and_differ_in_aad() {
        let key = SecretKey::generate();
        let all = [Purpose::Owner, Purpose::Authenticator, Purpose::DataKey, Purpose::Storage, Purpose::FunctionsDispatch];
        assert_eq!(Purpose::Owner.as_str(), "owner");
        assert_eq!(Purpose::FunctionsDispatch.as_str(), "functions_dispatch");
        assert_eq!(Purpose::Storage.as_str(), "storage");
        assert_eq!(Purpose::Authenticator.as_str(), "authenticator");
        assert_eq!(Purpose::DataKey.as_str(), "data_key");
        for sealed_for in all {
            let sealed = seal_credential(&key, "id-1", sealed_for, "pw");
            for open_as in all {
                let opened = open_credential(&key, "id-1", open_as, &sealed);
                if open_as == sealed_for {
                    assert_eq!(opened.unwrap(), "pw");
                } else {
                    assert!(matches!(opened, Err(SecretsError::Decrypt)), "{sealed_for:?} as {open_as:?}");
                }
            }
        }
        let sealed = seal_credential(&key, "id-1", Purpose::DataKey, "pw");
        assert_eq!(open(&key, "id-1:data_key", &sealed).unwrap(), "pw");
    }

    #[test]
    fn role_password_round_trips_and_binds_branch_and_name() {
        let key = SecretKey::generate();
        assert_eq!(Purpose::Role.as_str(), "role");
        let sealed = seal_role_password(&key, "b-1", "app", "pw");
        assert_eq!(open_role_password(&key, "b-1", "app", &sealed).unwrap(), "pw");
        assert!(matches!(open_role_password(&key, "b-2", "app", &sealed), Err(SecretsError::Decrypt)));
        assert!(matches!(open_role_password(&key, "b-1", "other", &sealed), Err(SecretsError::Decrypt)));
        assert_eq!(open(&key, "b-1:role:app", &sealed).unwrap(), "pw");
    }

    #[test]
    fn member_role_password_round_trips_and_binds_branch_and_name() {
        let key = SecretKey::generate();
        assert_eq!(Purpose::MemberRole.as_str(), "member_role");
        let (branch, other) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let sealed = seal_member_role_password(&key, branch, "cb_m_a", "pw").unwrap();
        assert_eq!(open_member_role_password(&key, branch, "cb_m_a", &sealed).unwrap(), "pw");
        assert!(matches!(
            open_member_role_password(&key, other, "cb_m_a", &sealed),
            Err(SecretsError::Decrypt)
        ));
        assert!(matches!(
            open_member_role_password(&key, branch, "cb_m_b", &sealed),
            Err(SecretsError::Decrypt)
        ));
        assert_eq!(open(&key, &format!("{branch}:member_role:cb_m_a"), &sealed).unwrap(), "pw");
        // A login role's sealed password never opens as a member role's, nor the reverse.
        let as_role = seal_role_password(&key, &branch.to_string(), "cb_m_a", "pw");
        assert!(open_member_role_password(&key, branch, "cb_m_a", &as_role).is_err());
        assert!(open_role_password(&key, &branch.to_string(), "cb_m_a", &sealed).is_err());
    }
}
