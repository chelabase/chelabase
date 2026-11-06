//! Tickets (spec §1, R9, P5, W15): 32 random bytes, base64url without padding, valid 30 s,
//! single use. The Valkey key is `rt:ticket:<hex sha256(ticket)>`; its value is the gateway's
//! verified identity sealed with AES-256-GCM under realtime's own key
//! (`CHELABASE_REALTIME_TICKET_KEY_FILE`, 32 raw bytes) with AAD = the ticket hash, so a value
//! copied under another ticket's key doesn't open. Redeemed with `GETDEL` before the 101.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chelabase_auth::internal::{AuthMethod, DataIdentity, InternalClaims};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::store::{StoreError, TicketStore};

/// How long a ticket lives.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// The ticket's random bytes, and its length as base64url without padding.
const TICKET_BYTES: usize = 32;
const TICKET_LEN: usize = 43;
const NONCE_LEN: usize = 12;

/// Who a ticket stands for: the gateway identity it was issued from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TicketIdentity {
    pub sub: String,
    /// The member's tenant when the gateway's identity came from a member token (#119).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<uuid::Uuid>,
    pub auth: AuthMethod,
    pub email: Option<String>,
    pub name: Option<String>,
    /// A data key's identity (RT2's app tickets); `None` for a Studio member.
    pub data: Option<DataIdentity>,
}

/// What a ticket's socket may do (R9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketKind {
    /// A Studio member (`ManagementJwt`): first frame `auth`, `table:`/`project:` only.
    Studio,
    /// An app (`DataKey`): never sends `auth`, `app:` only.
    App,
}

impl TicketIdentity {
    pub fn from_claims(claims: &InternalClaims) -> Self {
        Self {
            sub: claims.sub.clone(),
            tenant_id: claims.tenant_id,
            auth: claims.auth,
            email: claims.email.clone(),
            name: claims.name.clone(),
            data: claims.data.clone(),
        }
    }
    pub fn kind(&self) -> TicketKind {
        match self.auth {
            AuthMethod::ManagementJwt => TicketKind::Studio,
            AuthMethod::DataKey => TicketKind::App,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TicketError {
    /// No such ticket: never issued, expired, already used, or not a ticket's shape.
    #[error("unknown ticket")]
    Unknown,
    /// The stored value doesn't open under this ticket (tampered, or copied from another).
    #[error("the ticket doesn't open")]
    Forged,
    /// Valkey is down.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The sealing key isn't 32 bytes.
    #[error("the ticket key must be 32 bytes")]
    BadKey,
}

/// Realtime's AES-256-GCM sealing key.
#[derive(Clone)]
pub struct TicketKey {
    key: Key<Aes256Gcm>,
}

impl std::fmt::Debug for TicketKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TicketKey(..)")
    }
}

fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    bytes
}

impl TicketKey {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TicketError> {
        let key = Key::<Aes256Gcm>::try_from(bytes).map_err(|_| TicketError::BadKey)?;
        Ok(Self { key })
    }
    /// The file's raw bytes (exactly 32).
    pub fn from_file(path: &Path) -> Result<Self, TicketError> {
        // A missing or unreadable file is no key either; the caller names the path.
        let bytes = std::fs::read(path).map_err(|_| TicketError::BadKey)?;
        Self::from_bytes(&bytes)
    }
    /// A fresh random key (tests).
    pub fn generate() -> Self {
        Self::from_bytes(&random::<32>()).expect("32 bytes")
    }
    /// A random nonce, then the ciphertext of the identity's JSON, AAD = `hash`.
    pub fn seal(&self, hash: &str, identity: &TicketIdentity) -> Vec<u8> {
        let nonce_bytes = random::<NONCE_LEN>();
        let nonce = Nonce::try_from(&nonce_bytes[..]).expect("12 bytes");
        let plaintext = serde_json::to_vec(identity).expect("an identity serializes");
        let ciphertext = Aes256Gcm::new(&self.key)
            .encrypt(
                &nonce,
                Payload {
                    msg: &plaintext,
                    aad: hash.as_bytes(),
                },
            )
            .expect("AES-GCM encryption cannot fail for in-memory input");
        let mut out = nonce_bytes.to_vec();
        out.extend_from_slice(&ciphertext);
        out
    }
    pub fn unseal(&self, hash: &str, sealed: &[u8]) -> Result<TicketIdentity, TicketError> {
        if sealed.len() <= NONCE_LEN {
            return Err(TicketError::Forged);
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let nonce = Nonce::try_from(nonce).map_err(|_| TicketError::Forged)?;
        let plaintext = Aes256Gcm::new(&self.key)
            .decrypt(
                &nonce,
                Payload {
                    msg: ciphertext,
                    aad: hash.as_bytes(),
                },
            )
            .map_err(|_| TicketError::Forged)?;
        serde_json::from_slice(&plaintext).map_err(|_| TicketError::Forged)
    }
}

/// 32 random bytes, base64url without padding (43 characters).
pub fn new_ticket() -> String {
    URL_SAFE_NO_PAD.encode(random::<TICKET_BYTES>())
}

/// Whether `ticket` has a ticket's shape: 43 base64url characters that decode to 32 bytes.
fn well_formed(ticket: &str) -> bool {
    ticket.len() == TICKET_LEN
        && URL_SAFE_NO_PAD
            .decode(ticket)
            .is_ok_and(|b| b.len() == TICKET_BYTES)
}

/// Hex sha256 of the ticket string.
pub fn ticket_hash(ticket: &str) -> String {
    hex::encode(Sha256::digest(ticket.as_bytes()))
}

/// `rt:ticket:<hash>`.
pub fn store_key(hash: &str) -> String {
    format!("rt:ticket:{hash}")
}

/// Issues and redeems tickets.
#[derive(Clone)]
pub struct Tickets {
    key: TicketKey,
    store: Arc<dyn TicketStore>,
}

impl Tickets {
    pub fn new(key: TicketKey, store: Arc<dyn TicketStore>) -> Self {
        Self { key, store }
    }
    /// A new ticket, stored sealed for [`TICKET_TTL`].
    pub async fn issue(&self, identity: &TicketIdentity) -> Result<String, TicketError> {
        let ticket = new_ticket();
        let hash = ticket_hash(&ticket);
        let sealed = self.key.seal(&hash, identity);
        self.store
            .put(&store_key(&hash), sealed, TICKET_TTL)
            .await?;
        Ok(ticket)
    }
    /// The ticket's identity, consumed (`GETDEL`). A string that isn't a ticket's shape is
    /// [`TicketError::Unknown`] without a Valkey call.
    pub async fn redeem(&self, ticket: &str) -> Result<TicketIdentity, TicketError> {
        if !well_formed(ticket) {
            return Err(TicketError::Unknown);
        }
        let hash = ticket_hash(ticket);
        match self.store.take(&store_key(&hash)).await? {
            None => Err(TicketError::Unknown),
            Some(sealed) => self.key.unseal(&hash, &sealed),
        }
    }
}
