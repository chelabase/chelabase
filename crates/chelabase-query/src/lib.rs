//! Chelabase's data API query engine. Its pure core turns a JSON query tree into SQL:
//! [`query`] parses the tree, [`sql`] plans it against a [`sql::Catalog`] and [`shape`]
//! hashes its shape for the plan cache. Names in SQL come only from the catalog and are
//! always quoted; values are always bind parameters.
//!
//! Around it: [`schema`] introspects a database and decides what each role can reach,
//! [`engine`] runs a request (as a role, or as the database's owner) through the schema
//! and shape caches ([`QueryCache`]), and [`error`] maps failures to the data API's codes.
//! The crate is given a tokio-postgres client (its caller owns the pool; a pooled one caches
//! prepared statements, see [`QueryClient`]); [`run`] and [`schema_for`] open and commit
//! their own transaction ([`with_transaction`]), [`run`] in one pipelined round trip when
//! it can ([`pipeline`]).

pub mod app_users;
pub mod engine;
pub mod error;
#[cfg(test)]
mod fake_pg;
#[cfg(feature = "openapi")]
pub mod openapi;
pub mod pipeline;
pub mod query;
pub mod schema;
pub mod shape;
pub mod sql;

pub use engine::{
    QueryCache, RunAs, RunSettings, SchemaKey, load_schema, run, schema_for, with_transaction,
};
pub use error::QueryFailure;
pub use pipeline::{Lease, QueryClient};
pub use query::{Op, Parsed, QueryError, parse};
pub use schema::{RoleView, Schema, introspect, role_view, visibility};
pub use shape::shape_hash;
pub use sql::plan;
