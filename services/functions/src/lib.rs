//! The functions service (edge functions m4): runs a branch's functions in the runtime
//! (`infra/functions-runtime`) and serves their bundles to it.
//!
//! - The **public** listener takes the gateway's `/functions/v1/*` calls: it verifies the
//!   gateway's identity token, resolves the function on the caller's branch through a cache
//!   (TTL plus `chelabase.fn.changed.<branch>` from NATS), checks its `auth` against the caller
//!   ([`caller`]) and forwards the request to the runtime with the bundle and env headers.
//! - The **internal** listener, bound on the `functions` network only, serves bundles to the
//!   runtime (`GET`, no bearer), stores them for the orchestrator (`PUT`, its bearer) and invokes
//!   functions for the worker (`POST /internal/invoke`, its bearer).
//! - m9: the public listener also proxies `/services/v1/*` to external services
//!   ([`services`]), signed, health-checked, never to the stack's own network.

pub mod app;
pub mod bundles;
pub mod caller;
pub mod config;
pub mod directory;
pub mod host;
pub mod resolver;
pub mod services;
