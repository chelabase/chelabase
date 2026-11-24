//! Chelabase HTTP API gateway.
//!
//! `proxy` (and `main.rs`) adapt the Pingora engine; every other module is
//! engine-independent so it can be unit tested and survive an engine swap.

pub mod bad_bearers;
pub mod buckets;
pub mod client_addr;
pub mod config;
pub mod data_keys;
pub mod logging;
pub mod pipeline;
pub mod proxy;
pub mod registry;
pub mod request_limits;
pub mod upstreams;
pub mod user_tokens;
