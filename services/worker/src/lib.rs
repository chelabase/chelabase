//! Cheladb worker: provisioning, deletion, credential resets and reconcile.
//!
//! Pure decision logic lives in these modules so it can be tested without
//! Docker or Postgres; `main.rs` and `docker_ops` do the I/O.

pub mod credentials;
pub mod docker_ops;
pub mod names;
pub mod reconcile;
pub mod roles;
