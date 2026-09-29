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

/// File names of the key pair `cheladb dev bootstrap` writes to `.cheladb/neon/`.
pub const PRIVATE_KEY_FILE: &str = "compute-jwt.pem";
pub const PUBLIC_KEY_FILE: &str = "compute-jwt.pub";

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    compute_id: String,
    exp: u64,
}

pub struct ComputeSigner {
    key: EncodingKey,
    /// Base64url (no padding) Ed25519 public key: the JWK `x` parameter.
    public_x: String,
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
        let signer = Self {
            key,
            public_x: public_x.to_string(),
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

    fn verify(&self, token: &str) -> anyhow::Result<String> {
        let key = DecodingKey::from_ed_components(&self.public_x)?;
        let data = jsonwebtoken::decode::<Claims>(token, &key, &Validation::new(Algorithm::EdDSA))?;
        Ok(data.claims.compute_id)
    }
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
}
