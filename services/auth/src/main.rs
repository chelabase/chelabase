use std::sync::Arc;
use tracing::info;
use auth::app::{AppState, router};
use auth::config::Config;
use auth::guard::RateLimiter;
use auth::zitadel::Zitadel;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "auth=info".into()),
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
    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    info!("auth listening on {}", config.listen);
    let state = Arc::new(AppState {
        zitadel: Zitadel::new(&config.issuer, &config.token),
        config,
        limiter: RateLimiter::default(),
    });
    axum::serve(listener, router(state)).await?;
    Ok(())
}
