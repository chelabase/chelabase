//! Neon-facing building blocks for the Chelabase control plane: the storage-controller
//! client, the compute spec compute_ctl pulls, the proxy API bodies, compute containers,
//! SCRAM verifiers, LSNs, compute JWTs and ids.

pub mod backend;
pub mod compute;
pub mod id;
pub mod jwt;
pub mod lsn;
pub mod proxy_api;
pub mod proxy_cache;
pub mod reconfigure;
pub mod scram;
pub mod spec;
pub mod storcon;

pub use backend::{ComputeBackend, ComputeSettings, ComputeStatus, DockerBackend};
pub use compute::{ComputeLocator, DockerLocator};
pub use id::NeonId;
pub use jwt::ComputeSigner;
pub use lsn::Lsn;
pub use storcon::{StorconClient, StorconError};
