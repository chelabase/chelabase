use std::sync::Arc;
use tracing::info;
use auth::app::{AppState, router};
use auth::app_users::AppUsers;
use auth::config::{AppConfig, Config};
use auth::guard::RateLimiter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle("auth", "The Chelabase auth service: sign-in and app-user flows.", env!("CARGO_PKG_VERSION"));
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| auth::DEFAULT_LOG_FILTER.into()),
        )
        .init();
    let config = Config::from_env()?;
    info!(
        "mode: {}; email verification on sign-up: {}",
        config.mode.as_str(),
        if config.skip_email_verification {
            "off"
        } else {
            "required"
        }
    );
    let dev = chelabase_types::dev_defaults::DevDefaults::from_env().map_err(anyhow::Error::msg)?;
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    info!("auth listening on {}", config.listen);
    let app = match AppConfig::from_env()? {
        Some(c) => {
            AppConfig::check_internal_token(config.mode, dev, &c.internal_token)?;
            info!("app users' flows are on (access tokens live {} s)", c.access_ttl.as_secs());
            Some(AppUsers::from_config(&c)?)
        }
        None => None,
    };
    let signup_gate = {
        let c = AppConfig::internal_from_env()?;
        AppConfig::check_internal_token(config.mode, dev, &c.1)?;
        auth::app_users::InternalClient::new(&c.0, &c.1)
    };
    // Member tokens for the auth host's issuer, and the identity service's client: members can't
    // sign in without them, so neither is optional.
    let member_tokens = auth::config::MemberTokenConfig::from_env()?
        .map(|c| auth::member_token::MemberTokens::from_config(&c, &config.issuer))
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("CHELABASE_MEMBER_JWT_PRIVATE_KEY/PUBLIC_KEY are not set"))?;
    info!(kid = member_tokens.kid(), "member tokens are signed (they live {} s)", member_tokens.ttl().as_secs());
    let identity = auth::config::identity_from_lookup(|name| std::env::var(name).ok())?
        .ok_or_else(|| anyhow::anyhow!("CHELABASE_IDENTITY_URL/CHELABASE_IDENTITY_TOKEN_FILE are not set"))?;
    let state = Arc::new(AppState {
        app,
        signup_gate: Some(signup_gate),
        used_tickets: Default::default(),
        config,
        limiter: RateLimiter::default(),
        member_tokens,
        identity,
        clock: Default::default(),
    });
    // The TCP peer decides whose X-Forwarded-For counts (only the trusted proxy's).
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
