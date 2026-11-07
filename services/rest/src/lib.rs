//! The data API service. The query engine (grammar, schema cache, planner) is
//! [`chelabase_query`]; around it, [`conn`] finds and pools the branch's compute, [`exec`]
//! runs a request in one transaction, [`error`] maps failures to the API's error body and
//! [`app`] is the HTTP edge, whose public routes [`openapi`] documents.

pub mod app;
pub mod conn;
pub mod error;
pub mod exec;
pub mod openapi;

/// The log filter when `RUST_LOG` is unset: this service and the shared crates it links that
/// log, at `info`. Dependency crates (hyper, h2, deadpool, ...) stay out. `RUST_LOG` replaces it.
pub const DEFAULT_LOG_FILTER: &str = "rest=info,chelabase_query=info,chelabase_pgsession=info";

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
            assert_info_on!("rest", "chelabase_query", "chelabase_pgsession");
            assert!(!tracing::enabled!(target: "hyper", tracing::Level::INFO), "dependency crates stay out");
        });
    }
}
