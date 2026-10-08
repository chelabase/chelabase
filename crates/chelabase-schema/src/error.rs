//! The crate's error type.

use crate::steps::StepError;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SchemaError {
    /// The catalog came from a Postgres major other than [`crate::SUPPORTED_SERVER_MAJOR`].
    #[error("Postgres {0} isn't supported (schema merge reads Postgres {major})", major = crate::SUPPORTED_SERVER_MAJOR)]
    UnsupportedServer(u32),
    /// The catalog document isn't the JSON the reader expects.
    #[error("unreadable catalog document: {0}")]
    Parse(String),
    /// A catalog code the model has no value for (an `attgenerated` of `'v'`, …).
    #[error("unknown {what} in the catalog: {value:?}")]
    UnknownCatalogValue { what: String, value: String },
    /// User steps or own SQL that can't run in a merge.
    #[error("{} statement(s) can't run in a merge", .0.len())]
    Steps(Vec<StepError>),
    /// A change the planner can't express as DDL (removing or reordering enum values).
    #[error("{object}: {reason}")]
    Unplannable { object: String, reason: String },
}
