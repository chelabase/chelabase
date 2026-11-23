//! Who is calling: every `/mcp` request needs a member token (`chelabase_auth::member_token`),
//! verified here with `auth/member-jwt.pub` and the issuer. The gateway forwards the bearer
//! unchecked (m2, #346), so this is the only check. It reads headers only, so a bad bearer is
//! refused before the body is read or anything runs.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header};
use chelabase_auth::member_token::MemberTokenVerifier;
use uuid::Uuid;

/// The header `chelabase mcp --read-only` sends (m9, #353). It can only narrow a connection.
pub const READ_ONLY_HEADER: &str = "chelabase-mcp-read-only";
/// The least lifetime a token must have left: a tool run must not outlive its credential.
pub const MIN_REMAINING_SECS: u64 = 60;

/// The caller's raw bearer, kept for m11 (#355) to act as the member. Its `Debug` prints
/// nothing of it, so it cannot reach a log line through a derived `Debug`.
#[derive(Clone)]
pub struct Credential(String);

impl Credential {
    /// The raw token, for the one place that forwards it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credential(..)")
    }
}

/// What kind of credential the caller presented (epic `mcp-oauth` adds MCP access tokens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    Member,
}

impl CredentialKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CredentialKind::Member => "member",
        }
    }
}

/// The verified request context the tools receive.
#[derive(Debug, Clone)]
pub struct Caller {
    member: String,
    tenant: Uuid,
    kind: CredentialKind,
    read_only: bool,
    credential: Credential,
}

impl Caller {
    /// The member's identity user id (the token's `sub`).
    pub fn member(&self) -> &str {
        &self.member
    }

    pub fn tenant(&self) -> Uuid {
        self.tenant
    }

    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    /// Whether the connection is read-only (m11 enforces it).
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    #[cfg(test)]
    pub(crate) fn for_tests(member: &str, tenant: Uuid, read_only: bool) -> Self {
        Caller {
            member: member.into(),
            tenant,
            kind: CredentialKind::Member,
            read_only,
            credential: Credential("test-token".into()),
        }
    }
}

/// Why a request was refused. It never carries the token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Missing,
    Invalid,
    Expired,
    ExpiresSoon,
}

/// Checks `/mcp` bearers.
#[derive(Clone)]
pub struct Authenticator {
    verifier: MemberTokenVerifier,
    min_remaining: u64,
}

impl Authenticator {
    pub fn new(verifier: MemberTokenVerifier) -> Self {
        Self {
            verifier,
            min_remaining: MIN_REMAINING_SECS,
        }
    }

    /// Requires `secs` left on a token (never less than [`MIN_REMAINING_SECS`]): a run plus
    /// its margin must fit in what the credential has left (m11, #355).
    pub fn with_min_remaining(mut self, secs: u64) -> Self {
        self.min_remaining = secs.max(MIN_REMAINING_SECS);
        self
    }

    /// The least lifetime a token must have left.
    pub fn min_remaining(&self) -> u64 {
        self.min_remaining
    }

    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Caller, Refusal> {
        let token = bearer(headers).ok_or(Refusal::Missing)?;
        let claims = self.verifier.verify(token).map_err(|e| match e {
            chelabase_auth::member_token::MemberTokenError::Expired => Refusal::Expired,
            chelabase_auth::member_token::MemberTokenError::Invalid => Refusal::Invalid,
        })?;
        // The verifier allows 30 s of leeway; this check allows none.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| Refusal::Invalid)?;
        if claims.exp < now.saturating_add(self.min_remaining) {
            return Err(Refusal::ExpiresSoon);
        }
        // Only the exact value narrows, and any one of repeated headers is enough; nothing here
        // can widen a grant.
        let read_only = headers
            .get_all(READ_ONLY_HEADER)
            .iter()
            .any(|v| v.as_bytes() == b"true");
        Ok(Caller {
            member: claims.sub,
            tenant: claims.tid,
            kind: CredentialKind::Member,
            read_only,
            credential: Credential(token.to_owned()),
        })
    }
}

/// The token of a single `Authorization: Bearer <token>` header.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_of_a_credential_prints_no_token() {
        let token = "eyJhbGciOiJFZERTQSJ9.secret-part.sig";
        let caller = Caller {
            member: "m".into(),
            tenant: Uuid::nil(),
            kind: CredentialKind::Member,
            read_only: false,
            credential: Credential(token.into()),
        };
        for text in [
            format!("{:?}", caller.credential()),
            format!("{caller:?}"),
            format!("{caller:#?}"),
        ] {
            assert!(!text.contains("secret-part"), "{text}");
        }
    }
}
