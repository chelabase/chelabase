//! The storage service: buckets (and, from m5's next phase, objects) behind the gateway under
//! `/storage/v1`. Every operation runs its metadata statement in `storage.*` as the caller's
//! role and claims ([`engine`]), so row level security decides; [`error`] is the error body
//! (the data API's envelope), [`app`] the HTTP edge, [`buckets`] the bucket routes and
//! [`openapi`] their document.

pub mod app;
pub mod buckets;
pub mod cache;
pub mod config;
pub mod control;
pub mod engine;
pub mod error;
pub mod events;
pub mod internal;
pub mod limits;
pub mod multipart;
pub mod names;
pub mod objects;
pub mod openapi;
pub mod public;
pub mod signed;
pub mod token;
