//! S3 object-store abstraction for Chelabase: an [`ObjectStore`] trait, an S3 implementation
//! that works against SeaweedFS and Amazon S3, and a [`MemoryStore`] test double.
//!
//! The crate never assumes the store is local: endpoint, timeouts and retries are config, and
//! a connect failure is [`StorageError::Transient`].

pub mod config;
pub mod conformance;
pub mod memory;
pub mod s3;
mod store;

pub use config::{Backend, ConfigError, StorageConfig};
pub use memory::MemoryStore;
pub use s3::S3Store;
pub use store::{
    ByteRange, ByteStream, CompletedPart, GetObject, ObjectMeta, ObjectStore, PutOptions,
    StorageError, collect,
};
