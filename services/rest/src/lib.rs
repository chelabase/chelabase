//! The data API engine. This crate's pure core turns a JSON query tree into SQL:
//! [`query`] parses the tree, [`sql`] plans it against a [`sql::Catalog`] and [`shape`]
//! hashes its shape for the plan cache. Names in SQL come only from the catalog and are
//! always quoted; values are always bind parameters.
//!
//! Around it: [`schema`] introspects a branch's database and decides what each role can
//! reach, [`conn`] finds and pools the branch's compute, [`exec`] runs a request in one
//! transaction, [`error`] maps failures to the API's error body and [`app`] is the HTTP
//! edge.

pub mod app;
pub mod conn;
pub mod error;
pub mod exec;
pub mod query;
pub mod schema;
pub mod shape;
pub mod sql;
