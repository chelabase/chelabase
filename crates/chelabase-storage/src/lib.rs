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
    ByteRange, ByteStream, CompletedPart, DatedObject, GetObject, ObjectMeta, ObjectStore, OpenUpload, PutOptions,
    STREAM_PART_SIZE, StorageError, collect,
};

/// The longest a signed download URL may live (object-storage m6). It pins an object's
/// version, so the storage sweep (m8) keeps an unreferenced version longer than this.
pub const MAX_SIGNED_URL_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 86_400);
