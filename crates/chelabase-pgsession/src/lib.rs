//! Connections to a branch's compute, shared by the services that run statements there
//! (the data API, storage): [`pools`] finds the branch's compute and pools connections to
//! it within the branch's `max_connections`; [`session`] lends a connection as one role;
//! [`owner_level`] refuses owner-level roles; [`service`] runs a service's own named statements.

pub mod owner_level;
pub mod pools;
pub mod service;
pub mod session;

#[cfg(test)]
mod live_tests;

pub use pools::*;
pub use service::{NamedStatement, ServicePool, StatementText};
pub use session::{NoRole, RoleSession, run_settings};
