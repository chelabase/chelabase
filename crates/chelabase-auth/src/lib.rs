//! Authentication for Chelabase.
//!
//! - [`member_token`]: the member access tokens the auth service signs, verified locally.
//! - [`app_token`]: app users' access tokens.
//! - [`internal`]: gateway-minted internal identity tokens for service-to-service trust.

use thiserror::Error;

pub mod app_token;
pub mod member_token;
pub mod internal;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("auth config error: {0}")]
    Config(String),
    #[error("missing or invalid authorization bearer token")]
    Unauthorized,
    #[error("token verification failed: {0}")]
    InvalidToken(String),
    /// The token's `exp` has passed (beyond the leeway).
    #[error("token expired")]
    Expired,
}

/// Alias kept for existing call sites that only load config.
pub type AuthConfigError = AuthError;

#[cfg(feature = "test-support")]
pub mod test_support {
    //! Member tokens for tests: a fresh key pair, its verifier and a signer.

    use crate::internal::generate_keypair_pem;
    use crate::member_token::{
        MEMBER_AUDIENCE, MemberClaims, MemberTokenSigner, MemberTokenVerifier,
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    /// The issuer [`MemberKeys::new`] signs for unless a test names its own.
    pub const TEST_ISSUER: &str = "http://auth.test";

    /// A member-token key pair with its signer, for one test.
    pub struct MemberKeys {
        pub private_pem: String,
        pub public_pem: String,
        signer: MemberTokenSigner,
    }

    impl MemberKeys {
        /// A fresh pair whose tokens name [`TEST_ISSUER`].
        pub fn new() -> Self {
            Self::with_issuer(TEST_ISSUER)
        }

        /// A fresh pair whose tokens name `issuer`.
        pub fn with_issuer(issuer: &str) -> Self {
            let (private_pem, public_pem) = generate_keypair_pem();
            let signer =
                MemberTokenSigner::from_pem(private_pem.as_bytes(), public_pem.as_bytes(), issuer)
                    .expect("member signer");
            Self { private_pem, public_pem, signer }
        }

        pub fn issuer(&self) -> &str {
            self.signer.issuer()
        }

        /// The verifier for this pair and issuer.
        pub fn verifier(&self) -> MemberTokenVerifier {
            MemberTokenVerifier::from_pem(self.public_pem.as_bytes(), self.issuer())
                .expect("member verifier")
        }

        /// Well-formed claims for `sub` in `tid`, living `ttl_secs` from now (at most
        /// [`crate::member_token::MEMBER_MAX_TTL_SECS`] for a verifier to take it).
        pub fn claims(&self, sub: &str, tid: Uuid, ttl_secs: u64) -> MemberClaims {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_secs();
            MemberClaims {
                iss: self.issuer().to_string(),
                aud: MEMBER_AUDIENCE.to_string(),
                sub: sub.to_string(),
                tid,
                sid: format!("sid-{sub}"),
                email: format!("{sub}@example.com"),
                name: sub.to_string(),
                iat: now,
                exp: now + ttl_secs,
            }
        }

        /// A signed, currently valid member token for `sub` in `tid`.
        pub fn token(&self, sub: &str, tid: Uuid, ttl_secs: u64) -> String {
            self.sign(&self.claims(sub, tid, ttl_secs))
        }

        /// `claims` signed as they are.
        pub fn sign(&self, claims: &MemberClaims) -> String {
            self.signer.sign(claims).expect("sign member token")
        }
    }

    impl Default for MemberKeys {
        fn default() -> Self {
            Self::new()
        }
    }
}
