//! App users' flows (`/auth/v1/app/*`): sign-up, sign-in and sessions for a project's end
//! users, who live in the project's identity pool (`app-<project>`) and get Chelabase-signed
//! tokens.

pub mod flows;
pub mod identity;
pub mod internal;

pub use identity::{AppCaller, AppUserClaims};
pub use internal::{AppBranchInfo, InternalClient};

use crate::config::AppConfig;
use chelabase_auth::app_token::{APP_REFRESH_GRACE, AppTokenSigner};
use chelabase_auth::internal::InternalVerifier;
use chelabase_query::app_users::{PgUsersDb, UsersDb};
use chelabase_query::server_tls::ServerTls;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// How long a branch's settings are reused.
pub const BRANCH_CACHE_TTL: Duration = Duration::from_secs(30);
/// How long a refresh waits for identity's session answer before it answers 503 (#59 audit
/// B1). A refresh checks the session between the orchestrator's peek and its rotation, and the
/// orchestrator keeps a replaced token answerable for only [`APP_REFRESH_GRACE`] (10 s): a
/// request that peeked the current token just before a parallel one rotated it must still
/// reach `refresh` inside that window. So the lookup takes at most 2/5 of the grace (4 s),
/// leaving the rest (6 s) for the peek's reply, the `refresh` call and its row lock. Other
/// identity calls keep the client's own 10 s timeout.
pub const REFRESH_USER_LOOKUP: Duration =
    Duration::from_millis(APP_REFRESH_GRACE.as_millis() as u64 * 2 / 5);
const _: () = assert!(REFRESH_USER_LOOKUP.as_millis() * 2 <= APP_REFRESH_GRACE.as_millis());

pub struct AppUsers {
    pub verifier: InternalVerifier,
    pub internal: InternalClient,
    pub signer: AppTokenSigner,
    pub users_db: Arc<dyn UsersDb>,
    pub access_ttl: Duration,
    pub branches: moka::future::Cache<Uuid, Arc<AppBranchInfo>>,
    /// [`REFRESH_USER_LOOKUP`]; shorter in tests.
    pub refresh_user_lookup: Duration,
}

impl AppUsers {
    pub fn new(
        verifier: InternalVerifier,
        internal: InternalClient,
        signer: AppTokenSigner,
        users_db: Arc<dyn UsersDb>,
        access_ttl: Duration,
    ) -> Self {
        Self {
            verifier,
            internal,
            signer,
            users_db,
            access_ttl,
            branches: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(BRANCH_CACHE_TTL)
                .build(),
            refresh_user_lookup: REFRESH_USER_LOOKUP,
        }
    }

    pub fn from_config(c: &AppConfig) -> anyhow::Result<Self> {
        let verifier = InternalVerifier::from_pem_file(&c.verify_key)
            .map_err(|e| anyhow::anyhow!("CHELABASE_INTERNAL_VERIFY_KEY: {e}"))?;
        let signer = AppTokenSigner::from_pem_files(&c.private_key, &c.public_key)
            .map_err(|e| anyhow::anyhow!("app token keys: {e}"))?;
        // U15: the sync verifies every compute against the platform CA.
        let server_tls = ServerTls::from_ca_file(&c.compute_ca)
            .map_err(|e| anyhow::anyhow!("CHELABASE_COMPUTE_CA_FILE: {e}"))?;
        Ok(Self::new(
            verifier,
            InternalClient::new(&c.internal_url, &c.internal_token),
            signer,
            Arc::new(PgUsersDb::with_server_tls(server_tls)),
            c.access_ttl,
        ))
    }
}
