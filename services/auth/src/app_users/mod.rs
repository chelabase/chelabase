//! App users' flows (`/auth/v1/app/*`): sign-up, sign-in and sessions for a project's end
//! users, who live in the project's own Zitadel organization and get Chelabase-signed tokens.

pub mod flows;
pub mod identity;
pub mod internal;

pub use identity::{AppCaller, AppUserClaims};
pub use internal::{AppBranchInfo, InternalClient};

use crate::config::AppConfig;
use crate::zitadel::Zitadel;
use anyhow::Context;
use chelabase_auth::app_token::AppTokenSigner;
use chelabase_auth::internal::InternalVerifier;
use chelabase_query::app_users::{PgUsersDb, UsersDb};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Zitadel's `urlTemplate` for app users' emails: the code alone (R6, P13).
pub const CODE_TEMPLATE: &str = "{{.Code}}";
/// How long a branch's settings are reused.
pub const BRANCH_CACHE_TTL: Duration = Duration::from_secs(30);

pub struct AppUsers {
    pub verifier: InternalVerifier,
    pub internal: InternalClient,
    pub signer: AppTokenSigner,
    pub users_db: Arc<dyn UsersDb>,
    /// Admin token client, only for deleting a user after a failed sign-up commit (P32).
    pub zitadel_admin: Zitadel,
    pub access_ttl: Duration,
    pub branches: moka::future::Cache<Uuid, Arc<AppBranchInfo>>,
}

impl AppUsers {
    pub fn new(
        verifier: InternalVerifier,
        internal: InternalClient,
        signer: AppTokenSigner,
        users_db: Arc<dyn UsersDb>,
        zitadel_admin: Zitadel,
        access_ttl: Duration,
    ) -> Self {
        Self {
            verifier,
            internal,
            signer,
            users_db,
            zitadel_admin,
            access_ttl,
            branches: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(BRANCH_CACHE_TTL)
                .build(),
        }
    }

    pub fn from_config(c: &AppConfig, issuer: &str) -> anyhow::Result<Self> {
        let verifier = InternalVerifier::from_pem_file(&c.verify_key)
            .map_err(|e| anyhow::anyhow!("CHELABASE_INTERNAL_VERIFY_KEY: {e}"))?;
        let signer = AppTokenSigner::from_pem_files(&c.private_key, &c.public_key)
            .map_err(|e| anyhow::anyhow!("app token keys: {e}"))?;
        let admin_pat = std::fs::read_to_string(&c.admin_pat_file)
            .with_context(|| format!("read {}", c.admin_pat_file.display()))?;
        Ok(Self::new(
            verifier,
            InternalClient::new(&c.internal_url, &c.internal_token),
            signer,
            Arc::new(PgUsersDb::new()),
            Zitadel::new(issuer, admin_pat.trim()),
            c.access_ttl,
        ))
    }
}
