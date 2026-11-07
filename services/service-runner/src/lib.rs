//! The service runner (edge functions m10): the one component that starts member service
//! containers. It holds the Docker socket, so everything it does is narrow on purpose:
//!
//! - it acts only on containers labelled with its own stack ([`STACK_LABEL`]) **and** a service
//!   ([`SERVICE_LABEL`]), checked on every call, so an unlabelled container (or another stack's,
//!   or a compute) is "not found" even when its id is given;
//! - every container it creates is hardened the same way ([`docker::create_body`]): non-root,
//!   read-only root, every capability dropped, no new privileges, memory, CPU and pids limits, no
//!   host mount, device or namespace, and only the `services` network;
//! - its callers (the functions service and the worker) each present their own bearer.

pub mod app;
pub mod backend;
pub mod config;
pub mod docker;
pub mod reconcile;
pub mod runner;
pub mod spec;

/// The stack a container belongs to (the scheme computes use, `chelabase-neon`'s `STACK_LABEL`).
pub const STACK_LABEL: &str = "chelabase.dev/stack";
/// The service (a `kind = service` function's id) a container runs.
pub const SERVICE_LABEL: &str = "chelabase.dev/service-id";
/// The branch whose service it is (its secrets and enabled state are per branch).
pub const BRANCH_LABEL: &str = "chelabase.dev/branch-id";
/// The deployment it runs; a new active deployment replaces the container.
pub const DEPLOYMENT_LABEL: &str = "chelabase.dev/deployment-id";
/// The port the service listens on inside the container (routing reads it, m11).
pub const PORT_LABEL: &str = "chelabase.dev/port";
