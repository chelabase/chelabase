//! Cheladb worker: provisioning, deletion, credential resets and reconcile.
//!
//! [`Worker`] runs the lifecycle over the ports in [`ports`], so it can be tested with
//! in-memory fakes; `main.rs` wires it to the real metadata database, storage controller,
//! Docker, Valkey and compute_ctl.

use std::time::Duration;

use cheladb_secrets::SecretKey;

pub mod config;
pub mod credentials;
pub mod delete;
#[cfg(test)]
mod fakes;
pub mod names;
pub mod ports;
pub mod provision;
pub mod reconcile;
pub mod reset;

/// The role that owns every database, and whose password is revealed.
pub const OWNER_ROLE: &str = "postgres";

/// Everything the lifecycle needs.
pub struct Worker<M, S, C, N, P> {
    pub metadata: M,
    pub storage: S,
    pub computes: C,
    pub cache: N,
    pub pusher: P,
    pub key: SecretKey,
    /// `CHELADB_DB_OWNER_SUPERUSER` in self-hosted mode: the owner role becomes SUPERUSER.
    pub owner_superuser: bool,
    /// How often provisioning checks whether a new compute answers (500 ms in production).
    pub ready_poll: Duration,
    /// How long provisioning waits for a new compute (60 s in production).
    pub ready_timeout: Duration,
}
