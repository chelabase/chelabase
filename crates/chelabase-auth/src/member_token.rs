//! Member access tokens (#119): short-lived EdDSA JWTs the auth service signs for a Studio
//! member with `auth/member-jwt.key`, verified locally by the gateway, realtime and the
//! orchestrator with `auth/member-jwt.pub` (never a JWKS fetch: it can't fail open).
//!
//! A member token is pinned on every axis an app token (`auth/app-jwt.key`) or the gateway's
//! internal token could be confused on: `alg` = EdDSA, `kid` = the member key's, `iss` = the
//! auth host exactly, `aud` = [`MEMBER_AUDIENCE`] exactly (one string), the exact claim set
//! (an app token's `role` or `project` is refused), a non-nil `tid`, `iat` not in the future
//! and a lifetime of at most [`MEMBER_MAX_TTL_SECS`]. 30 s leeway.

use crate::AuthError;
use crate::app_token::{kid_for, raw_public, read};
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Every member token's audience; distinct from every app token's `chelabase-project:<id>`.
pub const MEMBER_AUDIENCE: &str = "chelabase-member";
pub const MEMBER_LEEWAY_SECS: u64 = 30;
/// The default lifetime the auth service signs.
pub const MEMBER_DEFAULT_TTL_SECS: u64 = 300;
/// The longest lifetime a signer may be configured with and a verifier accepts (`exp - iat`).
pub const MEMBER_MAX_TTL_SECS: u64 = 900;

/// A member token's claims, exactly: a token with any other claim is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemberClaims {
    /// The auth host's issuer URL (`CHELABASE_AUTH_HOST` with the public scheme).
    pub iss: String,
    pub aud: String,
    /// The identity service's user id (`members.identity_user_id`).
    pub sub: String,
    /// The tenant the member acts in. It only names the tenant: every verifier's consumer
    /// still requires an active membership there.
    pub tid: Uuid,
    /// The identity session the token was minted from.
    pub sid: String,
    pub email: String,
    pub name: String,
    pub iat: u64,
    pub exp: u64,
}

/// Why a member token was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberTokenError {
    Invalid,
    /// Expired more than [`MEMBER_LEEWAY_SECS`] ago.
    Expired,
}

/// Whether `token` claims to be a member token: its header parses and names EdDSA. Only a
/// shape check: the member verifier still decides every such token (an app token, an internal
/// token, a forgery under another key is refused there).
pub fn member_shaped(token: &str) -> bool {
    decode_header(token.trim()).is_ok_and(|h| h.alg == Algorithm::EdDSA)
}

pub struct MemberTokenSigner {
    key: EncodingKey,
    kid: String,
    issuer: String,
}

impl MemberTokenSigner {
    /// From the PKCS#8 private PEM and its SPKI public PEM (the public half gives `kid`), and
    /// the issuer every token names.
    pub fn from_pem(
        private_pem: &[u8],
        public_pem: &[u8],
        issuer: &str,
    ) -> Result<Self, AuthError> {
        let key = EncodingKey::from_ed_pem(private_pem)
            .map_err(|e| AuthError::Config(format!("member signing key: {e}")))?;
        if issuer.trim().is_empty() {
            return Err(AuthError::Config("member tokens need an issuer".into()));
        }
        Ok(Self {
            key,
            kid: kid_for(&raw_public(public_pem)?),
            issuer: issuer.to_string(),
        })
    }

    pub fn from_pem_files(private: &Path, public: &Path, issuer: &str) -> Result<Self, AuthError> {
        Self::from_pem(&read(private)?, &read(public)?, issuer)
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Sign `claims` as they are (the caller sets `iss`, `aud` and the times).
    pub fn sign(&self, claims: &MemberClaims) -> Result<String, AuthError> {
        let mut header = Header::new(Algorithm::EdDSA);
        header.kid = Some(self.kid.clone());
        encode(&header, claims, &self.key)
            .map_err(|e| AuthError::Config(format!("sign member token: {e}")))
    }
}

#[derive(Clone)]
pub struct MemberTokenVerifier {
    key: DecodingKey,
    kid: String,
    issuer: String,
}

impl MemberTokenVerifier {
    /// From the SPKI PEM Ed25519 public key and the issuer tokens must name.
    pub fn from_pem(public_pem: &[u8], issuer: &str) -> Result<Self, AuthError> {
        let key = DecodingKey::from_ed_pem(public_pem)
            .map_err(|e| AuthError::Config(format!("member verify key: {e}")))?;
        if issuer.trim().is_empty() {
            return Err(AuthError::Config("member tokens need an issuer".into()));
        }
        Ok(Self {
            key,
            kid: kid_for(&raw_public(public_pem)?),
            issuer: issuer.to_string(),
        })
    }

    pub fn from_pem_file(public: &Path, issuer: &str) -> Result<Self, AuthError> {
        Self::from_pem(&read(public)?, issuer)
    }

    /// `CHELABASE_MEMBER_JWT_PUBLIC_KEY` (a path) with the issuer from `CHELABASE_AUTH_HOST`
    /// (and the public scheme). `None` when the key variable is unset or blank: member tokens
    /// are then refused.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, AuthError> {
        let Some(path) = get("CHELABASE_MEMBER_JWT_PUBLIC_KEY").filter(|v| !v.trim().is_empty())
        else {
            return Ok(None);
        };
        let issuer = chelabase_types::entry::issuer_from_lookup(&get)
            .map_err(|e| AuthError::Config(e.to_string()))?
            .ok_or_else(|| {
                AuthError::Config(
                    "CHELABASE_MEMBER_JWT_PUBLIC_KEY needs CHELABASE_AUTH_HOST".into(),
                )
            })?;
        Self::from_pem_file(Path::new(path.trim()), &issuer).map(Some)
    }

    pub fn kid(&self) -> &str {
        &self.kid
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    pub fn verify(&self, token: &str) -> Result<MemberClaims, MemberTokenError> {
        let token = token.trim();
        let header = decode_header(token).map_err(|_| MemberTokenError::Invalid)?;
        if header.alg != Algorithm::EdDSA || header.kid.as_deref() != Some(self.kid.as_str()) {
            return Err(MemberTokenError::Invalid);
        }
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[MEMBER_AUDIENCE]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.leeway = MEMBER_LEEWAY_SECS;
        // `MemberClaims` holds `iss` and `aud` as single strings, so a list naming the right
        // one among others fails to deserialize; they are compared exactly again below.
        let claims = decode::<MemberClaims>(token, &self.key, &validation)
            .map(|d| d.claims)
            .map_err(|e| match e.kind() {
                ErrorKind::ExpiredSignature => MemberTokenError::Expired,
                _ => MemberTokenError::Invalid,
            })?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| MemberTokenError::Invalid)?;
        let sound = claims.iss == self.issuer
            && claims.aud == MEMBER_AUDIENCE
            && !claims.sub.is_empty()
            && !claims.tid.is_nil()
            // The session is what logout and a reset end (the orchestrator checks it).
            && !claims.sid.trim().is_empty()
            && claims.iat <= now + MEMBER_LEEWAY_SECS
            && claims.exp > claims.iat
            && claims.exp - claims.iat <= MEMBER_MAX_TTL_SECS;
        if !sound {
            return Err(MemberTokenError::Invalid);
        }
        Ok(claims)
    }
}
