pub mod app;
pub mod app_users;
pub mod config;
pub mod cookie;
pub mod guard;
pub mod member_token;
pub mod openapi;
pub mod session;

/// The log filter when `RUST_LOG` is unset: this service and the shared crates it links that
/// log, at `info`. Dependency crates (hyper, h2, deadpool, ...) stay out. `RUST_LOG` replaces it.
pub const DEFAULT_LOG_FILTER: &str = "auth=info,chelabase_query=info";

#[cfg(test)]
mod default_log_filter_tests {
    use tracing_subscriber::layer::SubscriberExt;

    /// `tracing::enabled!` needs a literal target, so the table is a macro list.
    macro_rules! assert_info_on {
        ($($target:literal),+) => { $(
            assert!(tracing::enabled!(target: $target, tracing::Level::INFO), "{} is off at info", $target);
        )+ };
    }

    #[test]
    fn default_log_filter_enables_info_for_its_own_and_shared_crates() {
        let filter = tracing_subscriber::EnvFilter::try_new(super::DEFAULT_LOG_FILTER).expect("the default filter parses");
        let subscriber = tracing_subscriber::registry().with(filter);
        tracing::subscriber::with_default(subscriber, || {
            assert_info_on!("auth", "chelabase_query");
            assert!(!tracing::enabled!(target: "hyper", tracing::Level::INFO), "dependency crates stay out");
        });
    }
}
