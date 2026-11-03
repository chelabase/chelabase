//! `auth` frames (spec §1, R9, S2, S14, W32): a Studio socket's first frame within 5 s, then one
//! per token renewal. The member token is verified as the gateway does (auth's member key);
//! its `sub` and tenant (its `tid`, #119) must be the ticket's (else
//! 4003); a token whose `exp ≤ now` closes 4001, and the socket closes 4001 at `exp` exactly
//! unless a renewal came (no leeway). A renewal that fails verification closes 4001 at once
//! (W32). The token's contents never reach a log (R22).

use async_trait::async_trait;
use chelabase_auth::member_token::{MemberTokenError, MemberTokenVerifier};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::ticket::TicketIdentity;

/// A verified member token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberToken {
    pub sub: String,
    /// A member token's tenant (`tid`, #119).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
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
    /// A bad signature, another issuer, no `sub` or tenant, not a JWT.
    #[error("the token is invalid")]
    Invalid,
    /// The verifier can't check it now.
    #[error("the token can't be checked now")]
    Unavailable,
}

#[async_trait]
pub trait TokenVerifier: Send + Sync + 'static {
    async fn verify(&self, token: &str) -> Result<MemberToken, TokenError>;

    /// Whether tokens can be checked now (the member verify key is configured); the internal
    /// `/health` reports it.
    fn is_ready(&self) -> bool {
        true
    }
}

/// The production verifier: auth's member tokens (`CHELABASE_MEMBER_JWT_PUBLIC_KEY`, #119)
/// with the auth host's issuer. Without the key every token is refused and it isn't ready.
pub struct MemberVerifier {
    member: Option<MemberTokenVerifier>,
}

impl MemberVerifier {
    pub fn new(member: Option<MemberTokenVerifier>) -> Self {
        Self { member }
    }
}

#[async_trait]
impl TokenVerifier for MemberVerifier {
    async fn verify(&self, token: &str) -> Result<MemberToken, TokenError> {
        let member = self.member.as_ref().ok_or(TokenError::Invalid)?;
        match member.verify(token) {
            Ok(c) => Ok(MemberToken {
                sub: c.sub,
                tenant_id: Some(c.tid),
                exp: c.exp,
                name: Some(c.name).filter(|n| !n.is_empty()),
                email: Some(c.email).filter(|e| !e.is_empty()),
            }),
            Err(MemberTokenError::Expired) => Err(TokenError::Expired),
            Err(MemberTokenError::Invalid) => Err(TokenError::Invalid),
        }
    }

    fn is_ready(&self) -> bool {
        self.member.is_some()
    }
}

impl MemberVerifier {
    /// [`TokenVerifier::is_ready`], callable without the trait in scope.
    pub fn is_ready(&self) -> bool {
        TokenVerifier::is_ready(self)
    }
}

/// What an `auth` frame's verified token means for its socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCheck {
    Ok,
    /// Another `sub` or tenant than the ticket's: close 4003.
    Mismatch,
    /// `exp ≤ now`: close 4001.
    Expired,
}

/// R9/S14 on a verified token (`now` in unix seconds). A mismatch wins over expiry.
pub fn check_auth_frame(ticket: &TicketIdentity, token: &MemberToken, now: u64) -> AuthCheck {
    if token.sub != ticket.sub || token.tenant_id != ticket.tenant_id {
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
