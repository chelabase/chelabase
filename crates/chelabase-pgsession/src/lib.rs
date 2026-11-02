//! Connections to a branch's compute, shared by the services that run statements there
//! (the data API, storage): [`pools`] finds the branch's compute and pools connections to
//! it within the branch's `max_connections`; [`owner_level`] refuses owner-level roles.

pub mod owner_level;
pub mod pools;

pub use pools::*;
