//! Chelabase's schema merge engine (step 6g).
//!
//! Reads one Postgres database's catalog into a normalized [`SchemaModel`] whose objects carry
//! their OIDs, compares a base with a target and a source (three-way), builds the merged model,
//! plans it in phases, exports it as `.sql` and compares the result for drift. Pure: no HTTP,
//! no metadata access; the worker runs the SQL this crate produces.
//!
//! Serde contract: the JSON field names of the model are what stored base snapshots and the
//! API hold. Every enum is externally tagged (no `untagged`, `tag = …` or `flatten`): the
//! orchestrator is built with `serde_json/arbitrary_precision`, which breaks numbers inside
//! buffered (untagged, internally tagged or flattened) types.

pub mod analysis;
pub mod catalog;
pub mod compare;
pub mod ddl;
pub mod error;
pub mod exclusions;
pub mod export;
pub mod fingerprint;
pub mod identity;
pub mod model;
pub mod plan;
pub mod review;
pub mod sql;
pub mod steps;
#[cfg(test)]
pub(crate) mod testkit;

pub use error::SchemaError;
pub use fingerprint::fingerprint;
pub use identity::{Direction, IdentityMap, IdentityPair, ObjAt, Side};
pub use model::*;

/// The model's shape version. A stored snapshot of another version is never trusted.
pub const MODEL_VERSION: u32 = 1;

/// The only Postgres major the catalog reader accepts.
pub const SUPPORTED_SERVER_MAJOR: u32 = 17;
