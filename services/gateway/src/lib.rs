//! Cheladb HTTP API gateway.
//!
//! `proxy` (and `main.rs`) adapt the Pingora engine; every other module is
//! engine-independent so it can be unit tested and survive an engine swap.

pub mod config;
pub mod data_keys;
pub mod pipeline;
pub mod proxy;
pub mod registry;
pub mod upstreams;
pub mod user_tokens;
