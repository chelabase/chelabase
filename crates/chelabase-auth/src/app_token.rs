//! App-user access tokens: short-lived EdDSA JWTs minted by the auth service for a
//! project's end users, verified by the gateway with the public key only.

use crate::AuthError;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{
    decode, decode_header, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

pub const APP_ISSUER: &str = "chelabase-auth";
pub const APP_LEEWAY_SECS: u64 = 30;
/// How long an app session's previous refresh token still gets the answer its rotation gave.
/// The orchestrator applies it; the auth service bounds its refresh user lookup by it (#59).
pub const APP_REFRESH_GRACE: Duration = Duration::from_secs(10);

/// The audience of a project's app tokens.
pub fn app_audience(project: Uuid) -> String {
    format!("chelabase-project:{project}")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppClaims {
    pub iss: String,
    pub aud: String,
    /// The app user.
    pub sub: Uuid,
    /// The session this token belongs to.
    pub sid: Uuid,
    pub project: Uuid,
    pub email: String,
    pub email_verified: bool,
    pub role: String,
    pub iat: u64,
    pub exp: u64,
}

/// Why an app token was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum AppTokenError {
    Invalid,
    Expired,
}

/// `hex(sha256(raw public key))[..16]`.
pub fn kid_for(raw_public: &[u8; 32]) -> String {
    hex::encode(Sha256::digest(raw_public))[..16].to_string()
}

/// The raw 32-byte key out of an Ed25519 SPKI PEM (44 bytes of DER; the key is the last 32).
pub(crate) fn raw_public(public_pem: &[u8]) -> Result<[u8; 32], AuthError> {
    let text = std::str::from_utf8(public_pem)
        .map_err(|e| AuthError::Config(format!("Ed25519 public key: {e}")))?;
    let body: String = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("-----"))
        .collect();
    let der = STANDARD
        .decode(body)
        .map_err(|e| AuthError::Config(format!("Ed25519 public key: {e}")))?;
    if der.len() != 44 {
        return Err(AuthError::Config(format!(
            "Ed25519 public key: expected a 44-byte SPKI, got {} bytes",
            der.len()
        )));
    }
    let mut raw = [0u8; 32];
    raw.copy_from_slice(&der[12..]);
    Ok(raw)
}

pub(crate) fn read(path: &Path) -> Result<Vec<u8>, AuthError> {
    std::fs::read(path).map_err(|e| AuthError::Config(format!("read {}: {e}", path.display())))
}

pub struct AppTokenSigner {
    key: EncodingKey,
    kid: String,
    x: [u8; 32],
}

impl AppTokenSigner {
    /// From a PKCS#8 private PEM and its SPKI public PEM (the public half gives `kid` and `x`).
    pub fn from_pem(private_pem: &[u8], public_pem: &[u8]) -> Result<Self, AuthError> {
        let key = EncodingKey::from_ed_pem(private_pem)
            .map_err(|e| AuthError::Config(format!("app signing key: {e}")))?;
        let x = raw_public(public_pem)?;
        Ok(Self { key, kid: kid_for(&x), x })
    }

    pub fn from_pem_files(private: &Path, public: &Path) -> Result<Self, AuthError> {
        Self::from_pem(&read(private)?, &read(public)?)
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn sign(&self, claims: &AppClaims) -> Result<String, AuthError> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        encode(&header, claims, &self.key)
            .map_err(|e| AuthError::Config(format!("sign app token: {e}")))
    }

    /// The public key as a JWKS document.
    pub fn jwks(&self) -> serde_json::Value {
        serde_json::json!({ "keys": [{
            "kty": "OKP",
            "crv": "Ed25519",
            "x": URL_SAFE_NO_PAD.encode(self.x),
            "kid": self.kid,
            "alg": "EdDSA",
            "use": "sig",
        }]})
    }
}

pub struct AppTokenVerifier {
    key: DecodingKey,
    kid: String,
}

impl AppTokenVerifier {
    /// From an SPKI PEM Ed25519 public key.
    pub fn from_pem(public_pem: &[u8]) -> Result<Self, AuthError> {
        let key = DecodingKey::from_ed_pem(public_pem)
            .map_err(|e| AuthError::Config(format!("app verify key: {e}")))?;
        Ok(Self { key, kid: kid_for(&raw_public(public_pem)?) })
    }

    pub fn from_pem_file(public: &Path) -> Result<Self, AuthError> {
        Self::from_pem(&read(public)?)
    }

    /// Verify signature, issuer, `aud = app_audience(project)` and expiry (30 s leeway).
    pub fn verify(&self, token: &str, project: Uuid) -> Result<AppClaims, AppTokenError> {
        let header = decode_header(token.trim()).map_err(|_| AppTokenError::Invalid)?;
        if header.kid.as_deref().is_some_and(|k| k != self.kid) {
            return Err(AppTokenError::Invalid);
        }
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[APP_ISSUER]);
        validation.set_audience(&[app_audience(project)]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.leeway = APP_LEEWAY_SECS;
        decode::<AppClaims>(token.trim(), &self.key, &validation)
            .map(|d| d.claims)
            .map_err(|e| match e.kind() {
                ErrorKind::ExpiredSignature => AppTokenError::Expired,
                _ => AppTokenError::Invalid,
            })
    }
}
