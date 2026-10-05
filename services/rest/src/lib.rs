//! The data API service. The query engine (grammar, schema cache, planner) is
//! [`chelabase_query`]; around it, [`conn`] finds and pools the branch's compute, [`exec`]
//! runs a request in one transaction, [`error`] maps failures to the API's error body and
//! [`app`] is the HTTP edge, whose public routes [`openapi`] documents.

pub mod app;
pub mod conn;
pub mod error;
pub mod exec;
pub mod openapi;
