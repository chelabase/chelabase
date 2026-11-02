//! Member access tokens (#119): the auth service signs them with `auth/member-jwt.key`
//! (`CHELABASE_MEMBER_JWT_PRIVATE_KEY`), for the auth host's issuer and
//! [`MEMBER_AUDIENCE`], living [`MemberTokenConfig::ttl`] (300 s unless set, at most 900 s).
//! The gateway, realtime and the orchestrator verify them with `auth/member-jwt.pub`.
//! Minting from a session (`/auth/v1/token`) is m8's; this is the signer it uses.

use crate::config::MemberTokenConfig;
use chelabase_auth::member_token::{MEMBER_AUDIENCE, MemberClaims, MemberTokenSigner};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Who a member token is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberSubject<'a> {
    /// The identity service's user id.
    pub user_id: &'a str,
    pub tenant: Uuid,
    /// The identity session the token comes from.
    pub session_id: &'a str,
    pub email: &'a str,
    pub name: &'a str,
}

pub struct MemberTokens {
    signer: MemberTokenSigner,
    ttl: Duration,
}

impl MemberTokens {
    /// From the configured key pair; every token names `issuer` (the auth host's).
    pub fn from_config(config: &MemberTokenConfig, issuer: &str) -> anyhow::Result<Self> {
        let signer =
            MemberTokenSigner::from_pem_files(&config.private_key, &config.public_key, issuer)
                .map_err(|e| anyhow::anyhow!("member tokens: {e}"))?;
        Ok(Self {
            signer,
            ttl: config.ttl,
        })
    }

    pub fn kid(&self) -> &str {
        self.signer.kid()
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// A token for `who`, issued now: the token and its `exp`.
    pub fn mint(&self, who: &MemberSubject<'_>) -> anyhow::Result<(String, u64)> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        self.mint_at(who, now)
    }

    fn mint_at(&self, who: &MemberSubject<'_>, now: u64) -> anyhow::Result<(String, u64)> {
        anyhow::ensure!(
            !who.user_id.is_empty() && !who.tenant.is_nil(),
            "a member token needs a user and a tenant"
        );
        let exp = now + self.ttl.as_secs();
        let claims = MemberClaims {
            iss: self.signer.issuer().to_string(),
            aud: MEMBER_AUDIENCE.to_string(),
            sub: who.user_id.to_string(),
            tid: who.tenant,
            sid: who.session_id.to_string(),
            email: who.email.to_string(),
            name: who.name.to_string(),
            iat: now,
            exp,
        };
        let token = self
            .signer
            .sign(&claims)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok((token, exp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_auth::app_token::AppTokenVerifier;
    use chelabase_auth::internal::generate_keypair_pem;
    use chelabase_auth::member_token::{MemberTokenError, MemberTokenVerifier};
    use std::path::PathBuf;

    const ISSUER: &str = "http://auth.chelabase.localhost:8000";

    fn setup(ttl: u64) -> (MemberTokens, String, tempfile::TempDir) {
        let (private, public) = generate_keypair_pem();
        let dir = tempfile::tempdir().unwrap();
        let (k, p) = (
            dir.path().join("member-jwt.key"),
            dir.path().join("member-jwt.pub"),
        );
        std::fs::write(&k, &private).unwrap();
        std::fs::write(&p, &public).unwrap();
        let config = MemberTokenConfig {
            private_key: k,
            public_key: p,
            ttl: Duration::from_secs(ttl),
        };
        (
            MemberTokens::from_config(&config, ISSUER).unwrap(),
            public,
            dir,
        )
    }

    fn ana(tenant: Uuid) -> MemberSubject<'static> {
        MemberSubject {
            user_id: "user_ana",
            tenant,
            session_id: "sess_1",
            email: "ana@example.com",
            name: "Ana Lima",
        }
    }

    // The signer's tokens are what every verifier accepts: the claims, the TTL, the kid.
    #[test]
    fn minted_tokens_verify_with_the_public_key() {
        let (tokens, public, _dir) = setup(300);
        let tenant = Uuid::new_v4();
        let (token, exp) = tokens.mint(&ana(tenant)).unwrap();
        let verifier = MemberTokenVerifier::from_pem(public.as_bytes(), ISSUER).unwrap();
        let claims = verifier.verify(&token).unwrap();
        assert_eq!(
            (
                claims.sub.as_str(),
                claims.tid,
                claims.sid.as_str(),
                claims.email.as_str(),
                claims.name.as_str()
            ),
            ("user_ana", tenant, "sess_1", "ana@example.com", "Ana Lima")
        );
        assert_eq!(
            (claims.aud.as_str(), claims.iss.as_str()),
            (MEMBER_AUDIENCE, ISSUER)
        );
        assert_eq!((claims.exp, claims.exp - claims.iat), (exp, 300));
        assert_eq!(tokens.kid(), verifier.kid());
    }

    // A member token is never an app token, even for an app verifier on the same key.
    #[test]
    fn minted_tokens_are_not_app_tokens() {
        let (tokens, public, _dir) = setup(300);
        let (token, _) = tokens.mint(&ana(Uuid::new_v4())).unwrap();
        let app = AppTokenVerifier::from_pem(public.as_bytes()).unwrap();
        assert!(app.verify(&token, Uuid::new_v4()).is_err());
    }

    #[test]
    fn expired_tokens_are_refused_past_the_leeway() {
        let (tokens, public, _dir) = setup(300);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (token, _) = tokens.mint_at(&ana(Uuid::new_v4()), now - 331).unwrap();
        let verifier = MemberTokenVerifier::from_pem(public.as_bytes(), ISSUER).unwrap();
        assert_eq!(verifier.verify(&token), Err(MemberTokenError::Expired));
    }

    #[test]
    fn no_token_without_a_user_or_tenant() {
        let (tokens, _, _dir) = setup(300);
        assert!(tokens.mint(&ana(Uuid::nil())).is_err());
        assert!(
            tokens
                .mint(&MemberSubject {
                    user_id: "",
                    ..ana(Uuid::new_v4())
                })
                .is_err()
        );
    }

    #[test]
    fn a_missing_key_file_refuses_to_start() {
        let config = MemberTokenConfig {
            private_key: PathBuf::from("/nonexistent/member-jwt.key"),
            public_key: PathBuf::from("/nonexistent/member-jwt.pub"),
            ttl: Duration::from_secs(300),
        };
        assert!(MemberTokens::from_config(&config, ISSUER).is_err());
    }
}
