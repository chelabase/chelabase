//! The storage service: buckets (and, from m5's next phase, objects) behind the gateway under
//! `/storage/v1`. Every operation runs its metadata statement in `storage.*` as the caller's
//! role and claims ([`engine`]), so row level security decides; [`error`] is the error body
//! (the data API's envelope), [`app`] the HTTP edge, [`buckets`] the bucket routes and
//! [`openapi`] their document.

pub mod app;
pub mod buckets;
pub mod cache;
pub mod config;
pub mod control;
pub mod engine;
pub mod error;
pub mod events;
pub mod internal;
pub mod limits;
pub mod multipart;
pub mod names;
pub mod objects;
pub mod openapi;
pub mod public;
pub mod signed;
pub mod token;

/// The log filter when `RUST_LOG` is unset: this service and the shared crates it links that
/// log, at `info`. Dependency crates (hyper, h2, deadpool, ...) stay out. `RUST_LOG` replaces it.
pub const DEFAULT_LOG_FILTER: &str = "storage=info,chelabase_query=info,chelabase_pgsession=info,chelabase_nats=info";

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
            assert_info_on!("storage", "chelabase_query", "chelabase_pgsession", "chelabase_nats");
            assert!(!tracing::enabled!(target: "hyper", tracing::Level::INFO), "dependency crates stay out");
        });
    }
}
