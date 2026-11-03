//! Gateway-minted internal identity tokens.
//!
//! The gateway verifies an external credential (a member token or a data key), then
//! signs a short-lived EdDSA JWT describing the caller for exactly one
//! upstream service (`aud`). Services verify it with the gateway's public key
//! and never talk to the identity provider. Asymmetric keys mean a service can
//! verify tokens but cannot mint them.

use crate::AuthError;
use chelabase_types::data_key::DataKeyKind;
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const INTERNAL_ISSUER: &str = "chelabase-gateway";
/// Request header carrying the internal token from the gateway to a service.
pub const IDENTITY_HEADER: &str = "x-chelabase-identity";
pub const INTERNAL_TOKEN_TTL_SECS: u64 = 60;
const LEEWAY_SECS: u64 = 5;

/// How the gateway authenticated the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthMethod {
    ManagementJwt,
    DataKey,
}

/// Who a data API key stands for: which branch, which key, and the Postgres role it runs as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataIdentity {
    pub branch_id: Uuid,
    pub key_id: Uuid,
    pub kind: DataKeyKind,
    pub role: String,
    /// Extra JWT-style claims for the role's policies; `{}` until end-user auth exists.
    pub claims: serde_json::Value,
    /// The databases the key may reach (its allow-list); `None` means every database the
    /// branch serves. Absent in tokens minted before allow-lists existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub databases: Option<Vec<String>>,
    /// The project an app-user token belongs to; absent for key-based identities and in
    /// tokens minted before app users existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// A secret key's role identity (`pg_roles.oid`): rest runs the request as `role` only
    /// while the role of that name has this OID. Absent for the public key, a user token, a
    /// key not backfilled yet, and in tokens minted before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_oid: Option<i64>,
    /// The app's server vouched for this app-auth call with the branch's secret key and sent
    /// no client address: auth skips its per-IP limits (its per-email and per-user ones still
    /// apply). Only the gateway sets it, only on `/auth/v1/app/*`; absent (false) in tokens
    /// minted before it existed and from every other minter.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub server_vouched: bool,
}

/// The caller, as established by the gateway. For a data key, `sub` is the key id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub sub: String,
    pub auth: AuthMethod,
    pub request_id: String,
    pub data: Option<DataIdentity>,
    /// The person's email and name from the management token, when it carries them (members
    /// are refreshed from these). Always `None` for a data key.
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalClaims {
    pub iss: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    pub sub: String,
    /// The member's tenant, from a member token's `tid` (#119). `None` for a data key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    pub auth: AuthMethod,
    /// Gateway request id, for following one request across services.
    pub rid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<DataIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The member token's identity session (`sid`): the orchestrator checks it is still live.
    /// Always set with `tenant_id`; `None` for a data key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
}

pub struct InternalSigner {
    key: EncodingKey,
}

impl InternalSigner {
    /// From a PKCS#8 PEM Ed25519 private key.
    pub fn from_pem(pem: &[u8]) -> Result<Self, AuthError> {
        EncodingKey::from_ed_pem(pem)
            .map(|key| Self { key })
            .map_err(|e| AuthError::Config(format!("internal signing key: {e}")))
    }

    pub fn from_pem_file(path: &Path) -> Result<Self, AuthError> {
        Self::from_pem(&read_key(path)?)
    }

    /// From the file named by `CHELABASE_INTERNAL_SIGNING_KEY`.
    pub fn from_env() -> Result<Self, AuthError> {
        Self::from_pem_file(Path::new(&env_path("CHELABASE_INTERNAL_SIGNING_KEY")?))
    }

    pub fn sign(&self, audience: &str, identity: &Identity) -> Result<String, AuthError> {
        self.sign_at(audience, identity, now_secs())
    }

    /// Sign with an explicit issue time (seconds since the Unix epoch).
    pub fn sign_at(
        &self,
        audience: &str,
        identity: &Identity,
        issued_at: u64,
    ) -> Result<String, AuthError> {
        self.sign_claims(audience, identity, None, issued_at)
    }

    /// Sign for a member whose token named `tenant` (`tid`, #119) and identity session `sid`;
    /// never the nil tenant, never an empty session.
    pub fn sign_for_tenant(
        &self,
        audience: &str,
        identity: &Identity,
        tenant: Uuid,
        sid: &str,
    ) -> Result<String, AuthError> {
        if tenant.is_nil() {
            return Err(AuthError::Config("a member identity names a tenant".into()));
        }
        if sid.trim().is_empty() {
            return Err(AuthError::Config("a member identity names its session".into()));
        }
        self.sign_claims(audience, identity, Some((tenant, sid)), now_secs())
    }

    fn sign_claims(
        &self,
        audience: &str,
        identity: &Identity,
        member: Option<(Uuid, &str)>,
        issued_at: u64,
    ) -> Result<String, AuthError> {
        let claims = InternalClaims {
            tenant_id: member.map(|(tenant, _)| tenant),
            sid: member.map(|(_, sid)| sid.to_string()),
            iss: INTERNAL_ISSUER.to_string(),
            aud: audience.to_string(),
            iat: issued_at,
            exp: issued_at + INTERNAL_TOKEN_TTL_SECS,
            sub: identity.sub.clone(),
            auth: identity.auth,
            rid: identity.request_id.clone(),
            data: identity.data.clone(),
            email: identity.email.clone(),
            name: identity.name.clone(),
        };
        encode(&Header::new(Algorithm::EdDSA), &claims, &self.key)
            .map_err(|e| AuthError::Config(format!("sign internal token: {e}")))
    }
}

pub struct InternalVerifier {
    key: DecodingKey,
}

impl InternalVerifier {
    /// From an SPKI PEM Ed25519 public key.
    pub fn from_pem(pem: &[u8]) -> Result<Self, AuthError> {
        DecodingKey::from_ed_pem(pem)
            .map(|key| Self { key })
            .map_err(|e| AuthError::Config(format!("internal verify key: {e}")))
    }

    pub fn from_pem_file(path: &Path) -> Result<Self, AuthError> {
        Self::from_pem(&read_key(path)?)
    }

    /// From the file named by `CHELABASE_INTERNAL_VERIFY_KEY`.
    pub fn from_env() -> Result<Self, AuthError> {
        Self::from_pem_file(Path::new(&env_path("CHELABASE_INTERNAL_VERIFY_KEY")?))
    }

    /// Verify signature, issuer, expiry, and that the token was minted for `audience`.
    pub fn verify(&self, token: &str, audience: &str) -> Result<InternalClaims, AuthError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(AuthError::Unauthorized);
        }
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[INTERNAL_ISSUER]);
        validation.set_audience(&[audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.leeway = LEEWAY_SECS;
        let claims = decode::<InternalClaims>(token, &self.key, &validation)
            .map(|data| data.claims)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;
        // #119: a member never names the nil tenant.
        if claims.tenant_id.is_some_and(|t| t.is_nil()) {
            return Err(AuthError::InvalidToken("an identity names the nil tenant".into()));
        }
        Ok(claims)
    }
}

/// A fresh Ed25519 keypair as (PKCS#8 private PEM, SPKI public PEM).
#[cfg(feature = "keygen")]
pub fn generate_keypair_pem() -> (String, String) {
    use ed25519_dalek::pkcs8::spki::der::pem::LineEnding;
    use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey};

    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).expect("OS random number generator");
    let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
    let private = signing
        .to_pkcs8_pem(LineEnding::LF)
        .expect("encode ed25519 private key")
        .to_string();
    let public = signing
        .verifying_key()
        .to_public_key_pem(LineEnding::LF)
        .expect("encode ed25519 public key");
    (private, public)
}

fn env_path(var: &str) -> Result<String, AuthError> {
    std::env::var(var).map_err(|_| {
        AuthError::Config(format!(
            "{var} is not set; a workbench stack sets it in its stack.env (`just workbench env` prints the path)"
        ))
    })
}

fn read_key(path: &Path) -> Result<Vec<u8>, AuthError> {
    std::fs::read(path).map_err(|e| AuthError::Config(format!("read {}: {e}", path.display())))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
