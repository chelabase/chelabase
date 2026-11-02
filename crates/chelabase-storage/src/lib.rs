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

pub use config::{Backend, ConfigError, CredentialSource, StorageConfig};
pub use memory::MemoryStore;
pub use s3::S3Store;
pub use store::{
    ByteRange, ByteStream, CompletedPart, GetObject, ObjectMeta, ObjectStore, PutOptions,
    COPY_OBJECT_MAX, COPY_PART_SIZE, STREAM_PART_SIZE, StorageError, collect, copy_in_parts,
};
