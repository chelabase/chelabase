//! `auth` frames (spec §1, R9, S2, S14, W32): a Studio socket's first frame within 5 s, then one
//! per oidc-client renewal. The token is verified as the gateway does (the same JWKS settings);
//! its `sub` and `org_id` must be the ticket's (else 4003); a token whose `exp ≤ now` closes
//! 4001, and the socket closes 4001 at `exp` exactly unless a renewal came (no leeway). A
//! renewal that fails verification closes 4001 at once (W32). The token's contents never reach
//! a log (R22).

use async_trait::async_trait;
use chelabase_auth::{AuthError, JwtVerifier, ZitadelAuthConfig};
use serde::{Deserialize, Serialize};

use crate::ticket::TicketIdentity;

/// A verified member token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberToken {
    pub sub: String,
    pub org_id: String,
    /// Unix seconds.
    pub exp: u64,
    pub name: Option<String>,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// Expired past the verifier's leeway.
    #[error("the token expired")]
    Expired,
    /// A bad signature, another issuer, no `sub` or org, not a JWT.
    #[error("the token is invalid")]
    Invalid,
    /// The JWKS can't be fetched (or the verifier is misconfigured).
    #[error("the token can't be checked now")]
    Unavailable,
}

#[async_trait]
pub trait TokenVerifier: Send + Sync + 'static {
    async fn verify(&self, token: &str) -> Result<MemberToken, TokenError>;

    /// Whether tokens can be checked now (#22: the audiences may come late from the
    /// bootstrap's files); the internal `/health` reports it.
    fn is_ready(&self) -> bool {
        true
    }
}

/// The production verifier: `chelabase_auth::JwtVerifier` with the gateway's settings.
pub struct OidcVerifier {
    inner: JwtVerifier,
}

impl OidcVerifier {
    pub fn new(config: ZitadelAuthConfig) -> Self {
        Self {
            inner: JwtVerifier::new(config),
        }
    }
}

#[async_trait]
impl TokenVerifier for OidcVerifier {
    async fn verify(&self, token: &str) -> Result<MemberToken, TokenError> {
        match self.inner.verify(token).await {
            Ok(c) => Ok(MemberToken {
                sub: c.sub,
                org_id: c.org_id,
                exp: c.exp,
                name: c.name,
                email: c.email,
            }),
            Err(AuthError::Expired) => Err(TokenError::Expired),
            Err(AuthError::Unauthorized | AuthError::InvalidToken(_)) => Err(TokenError::Invalid),
            Err(AuthError::Jwks(_) | AuthError::Config(_) | AuthError::NotReady(_)) => {
                Err(TokenError::Unavailable)
            }
        }
    }

    fn is_ready(&self) -> bool {
        self.inner.config().audiences.is_ready()
    }
}

impl OidcVerifier {
    /// [`TokenVerifier::is_ready`], callable without the trait in scope.
    pub fn is_ready(&self) -> bool {
        TokenVerifier::is_ready(self)
    }
}

/// What an `auth` frame's verified token means for its socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCheck {
    Ok,
    /// Another `sub` or `org_id` than the ticket's: close 4003.
    Mismatch,
    /// `exp ≤ now`: close 4001.
    Expired,
}

/// R9/S14 on a verified token (`now` in unix seconds). A mismatch wins over expiry.
pub fn check_auth_frame(ticket: &TicketIdentity, token: &MemberToken, now: u64) -> AuthCheck {
    if token.sub != ticket.sub || token.org_id != ticket.org_id {
        AuthCheck::Mismatch
    } else if token.exp <= now {
        AuthCheck::Expired
    } else {
        AuthCheck::Ok
    }
}

pub mod fake {
    //! A verifier for tests. A token is `fake.` + base64url(JSON of a [`MemberToken`]); verify
    //! returns it as is (whatever its `exp`: the socket's own check decides), anything else is
    //! [`TokenError::Invalid`], and while [`FakeVerifier::set_unavailable`] is on every call is
    //! [`TokenError::Unavailable`].

    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    use super::{MemberToken, TokenError, TokenVerifier};

    #[derive(Default)]
    pub struct FakeVerifier {
        unavailable: AtomicBool,
        not_ready: AtomicBool,
    }

    impl FakeVerifier {
        pub fn new() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self::default())
        }
        pub fn token(member: &MemberToken) -> String {
            let json = serde_json::to_vec(member).expect("a member token serializes");
            format!("fake.{}", URL_SAFE_NO_PAD.encode(json))
        }
        pub fn decode(token: &str) -> Option<MemberToken> {
            let body = token.strip_prefix("fake.")?;
            let json = URL_SAFE_NO_PAD.decode(body).ok()?;
            serde_json::from_slice(&json).ok()
        }
        pub fn set_unavailable(&self, on: bool) {
            self.unavailable.store(on, Ordering::SeqCst);
        }
        /// What [`TokenVerifier::is_ready`] answers (ready unless set).
        pub fn set_not_ready(&self, on: bool) {
            self.not_ready.store(on, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl TokenVerifier for FakeVerifier {
        async fn verify(&self, token: &str) -> Result<MemberToken, TokenError> {
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(TokenError::Unavailable);
            }
            Self::decode(token).ok_or(TokenError::Invalid)
        }

        fn is_ready(&self) -> bool {
            !self.not_ready.load(Ordering::SeqCst)
        }
    }
}
