//! Tokens the control plane presents to compute_ctl (e.g. on `POST /configure`).
//!
//! compute_ctl verifies EdDSA tokens against the JWKS in its `compute_ctl_config`
//! (neon `compute_tools/src/http/middleware/authorize.rs`): `exp` is checked, and a
//! token without the admin scope must carry the compute's own `compute_id`.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const TOKEN_TTL_SECS: u64 = 300;

/// File names of the key pair `chelabase dev bootstrap` writes to `.chelabase/neon/`.
pub const PRIVATE_KEY_FILE: &str = "compute-jwt.pem";
pub const PUBLIC_KEY_FILE: &str = "compute-jwt.pub";

/// The public key as SPKI PEM, derived from [`PUBLIC_KEY_FILE`] by the workbench: the pageserver,
/// safekeepers and storage controller verify storage tokens against it.
pub const PUBLIC_PEM_FILE: &str = "compute-jwt.pub.pem";

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    compute_id: String,
    exp: u64,
}

/// What a storage token opens (Neon's `utils::auth::Scope`; the pageserver, safekeepers and
/// storage controller check it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageScope {
    /// One tenant's data on the pageserver and safekeepers: what a compute holds.
    Tenant(crate::NeonId),
    /// Every pageserver API (the storage controller's calls).
    PageServerApi,
    /// Every safekeeper API (the pageserver's WAL reads, the storage controller, peers).
    SafekeeperData,
    /// The pageserver's upcalls to the storage controller.
    GenerationsApi,
    /// Every storage controller route.
    Admin,
    /// The storage controller's infrastructure routes (node registration).
    Infra,
}

impl StorageScope {
    fn name(&self) -> &'static str {
        match self {
            StorageScope::Tenant(_) => "tenant",
            StorageScope::PageServerApi => "pageserverapi",
            StorageScope::SafekeeperData => "safekeeperdata",
            StorageScope::GenerationsApi => "generations_api",
            StorageScope::Admin => "admin",
            StorageScope::Infra => "infra",
        }
    }
}

/// Neon's storage claims: no `exp` (a compute presents its token on every reconnect for its
/// whole life), no `compute_id` or `aud` (compute_ctl never takes one), `tenant_id` only on a
/// tenant token.
#[derive(Serialize)]
struct StorageClaims<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant_id: Option<String>,
    scope: &'a str,
}

pub struct ComputeSigner {
    key: EncodingKey,
    /// Base64url (no padding) Ed25519 public key: the JWK `x` parameter.
    public_x: String,
    /// The same public key as SPKI PEM.
    public_pem: String,
    kid: String,
}

impl ComputeSigner {
    /// `private_pem`: PKCS#8 Ed25519 key. `public_x`: its public key, base64url.
    /// Fails if the two don't belong together.
    pub fn new(private_pem: &str, public_x: &str) -> anyhow::Result<Self> {
        let key = EncodingKey::from_ed_pem(private_pem.as_bytes())
            .context("parsing Ed25519 private key PEM")?;
        let public_x = public_x.trim();
        // From the trimmed key, so a trailing newline in a key file can't change the kid.
        let kid = hex::encode(&Sha256::digest(public_x.as_bytes())[..8]);
        let public_pem = spki_pem(public_x)?;
        let signer = Self {
            key,
            public_x: public_x.to_string(),
            public_pem,
            kid,
        };
        let token = signer.token_for("self-test")?;
        signer
            .verify(&token)
            .context("JWT private key does not match the configured public key")?;
        Ok(signer)
    }

    /// The JWKS to embed in `compute_ctl_config.jwks`.
    pub fn jwks(&self) -> Value {
        json!({ "keys": [{
            "kty": "OKP",
            "crv": "Ed25519",
            "x": self.public_x,
            "kid": self.kid,
            "alg": "EdDSA",
            "use": "sig",
        }]})
    }

    /// A short-lived token only the given compute accepts.
    pub fn token_for(&self, compute_id: &str) -> anyhow::Result<String> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        let claims = Claims {
            compute_id: compute_id.to_string(),
            exp: now + TOKEN_TTL_SECS,
        };
        Ok(jsonwebtoken::encode(&header, &claims, &self.key)?)
    }

    /// The public key as SPKI PEM (`-----BEGIN PUBLIC KEY-----`), as Neon's `JwtAuth` reads it.
    pub fn public_key_pem(&self) -> &str {
        &self.public_pem
    }

    /// A storage token for `scope`, without expiry. Ed25519 signatures are deterministic, so
    /// the same scope always gives the same token.
    pub fn storage_token(&self, scope: &StorageScope) -> anyhow::Result<String> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        let claims = StorageClaims {
            tenant_id: match scope {
                StorageScope::Tenant(tenant) => Some(tenant.to_string()),
                _ => None,
            },
            scope: scope.name(),
        };
        Ok(jsonwebtoken::encode(&header, &claims, &self.key)?)
    }

    fn verify(&self, token: &str) -> anyhow::Result<String> {
        let key = DecodingKey::from_ed_components(&self.public_x)?;
        let data = jsonwebtoken::decode::<Claims>(token, &key, &Validation::new(Algorithm::EdDSA))?;
        Ok(data.claims.compute_id)
    }
}

/// The SPKI PEM of a base64url (no padding) Ed25519 public key.
fn spki_pem(public_x: &str) -> anyhow::Result<String> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::VerifyingKey;
    use ed25519_dalek::pkcs8::EncodePublicKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;

    let bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(public_x)
        .context("the public key isn't base64url")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the public key isn't 32 bytes"))?;
    let key = VerifyingKey::from_bytes(&bytes).context("the public key isn't an Ed25519 point")?;
    key.to_public_key_pem(LineEnding::LF)
        .context("encoding the public key as PEM")
}

/// A fresh Ed25519 key pair for [`ComputeSigner::new`]: the PKCS#8 PEM private key and the
/// base64url (no padding) public key.
pub fn generate_keypair() -> (String, String) {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use ed25519_dalek::SigningKey;
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;

    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("OS random number generator");
    let key = SigningKey::from_bytes(&seed);
    let pem = key
        .to_pkcs8_pem(LineEnding::LF)
        .expect("PKCS#8 PEM")
        .to_string();
    (pem, URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()))
}

/// A storage token for `scope`, signed with the key pair in `dir` (live tests and tools that
/// call the storage controller or the safekeepers).
pub fn storage_token_from_dir(dir: &std::path::Path, scope: &StorageScope) -> anyhow::Result<String> {
    load_signer(dir)?.storage_token(scope)
}

/// The key pair `chelabase dev bootstrap` writes into `dir`.
pub fn load_signer(dir: &std::path::Path) -> anyhow::Result<ComputeSigner> {
    let read = |name: &str| {
        let path = dir.join(name);
        std::fs::read_to_string(&path).with_context(|| {
            format!(
                "read {} (the compute-JWT key pair; run just zitadel-bootstrap)",
                path.display()
            )
        })
    };
    ComputeSigner::new(&read(PRIVATE_KEY_FILE)?, &read(PUBLIC_KEY_FILE)?).with_context(|| {
        format!(
            "the compute-JWT key pair in {} is corrupt; delete both files and run \
             just zitadel-bootstrap",
            dir.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signs_tokens_verifiable_with_the_published_jwk() {
        let (pem, x) = generate_keypair();
        let signer = ComputeSigner::new(&pem, &x).unwrap();
        let token = signer.token_for("ep-abc").unwrap();
        // Verify the way compute_ctl does: a DecodingKey built from the JWK itself.
        let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(signer.jwks()).unwrap();
        let key = DecodingKey::from_jwk(&jwks.keys[0]).unwrap();
        let data = jsonwebtoken::decode::<Claims>(&token, &key, &Validation::new(Algorithm::EdDSA))
            .unwrap();
        assert_eq!(data.claims.compute_id, "ep-abc");
        assert_eq!(
            jsonwebtoken::decode_header(&token).unwrap().kid.as_deref(),
            jwks.keys[0].common.key_id.as_deref()
        );
    }

    #[test]
    fn surrounding_whitespace_in_the_public_key_does_not_change_the_kid() {
        let (pem, x) = generate_keypair();
        let kid = |x: &str| ComputeSigner::new(&pem, x).unwrap().jwks()["keys"][0]["kid"].clone();
        assert_eq!(kid(&format!("{x}\n")), kid(&x));
    }

    #[test]
    fn rejects_a_mismatched_public_key() {
        let (pem, _) = generate_keypair();
        let (_, other_x) = generate_keypair();
        assert!(ComputeSigner::new(&pem, &other_x).is_err());
    }

    #[test]
    fn rejects_a_malformed_private_key() {
        let (_, x) = generate_keypair();
        assert!(ComputeSigner::new("not a pem", &x).is_err());
    }

    #[test]
    fn generated_key_pairs_are_fresh_and_work() {
        let (pem_a, x_a) = generate_keypair();
        let (_, x_b) = generate_keypair();
        assert_ne!(x_a, x_b);
        assert_eq!(x_a.len(), 43, "32 bytes, base64url without padding");
        assert!(ComputeSigner::new(&pem_a, &x_a).is_ok());
    }

    #[test]
    fn load_signer_missing_keys_say_to_run_the_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let err = format!("{:#}", load_signer(dir.path()).err().expect("no keys"));
        assert!(err.contains("just zitadel-bootstrap"), "{err}");
    }

    #[test]
    fn load_signer_corrupt_keys_say_to_delete_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PRIVATE_KEY_FILE), "not a key").unwrap();
        std::fs::write(dir.path().join(PUBLIC_KEY_FILE), "not a key").unwrap();
        let err = format!("{:#}", load_signer(dir.path()).err().expect("corrupt keys"));
        assert!(err.contains("corrupt"), "{err}");
        assert!(err.contains("just zitadel-bootstrap"), "{err}");
    }

    #[test]
    fn load_signer_reads_the_bootstrap_key_pair() {
        let dir = tempfile::tempdir().unwrap();
        let (pem, x) = generate_keypair();
        std::fs::write(dir.path().join(PRIVATE_KEY_FILE), pem).unwrap();
        std::fs::write(dir.path().join(PUBLIC_KEY_FILE), format!("{x}\n")).unwrap();
        assert!(load_signer(dir.path()).is_ok());
    }
}
