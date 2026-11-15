//! The bundle store: `{database_id}/{sha256}` in the functions bucket, bytes stored and served
//! exactly as uploaded (the runtime hashes them as served), with a small in-memory cache.

use std::sync::Arc;

use bytes::Bytes;
use chelabase_storage::{ObjectStore, PutOptions, StorageError};
use moka::future::Cache;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::directory::Directory;

/// A bundle may be at most 10 MiB (`chelabase_metadata::functions::MAX_BUNDLE_SIZE`).
pub const MAX_BUNDLE_BYTES: usize = 10 * 1024 * 1024;
/// Bytes of bundles the cache holds.
const CACHE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum Put {
    Stored,
    /// The body doesn't hash to the path's sha256 (422).
    Mismatch,
    /// The database is unknown or tombstoned (404).
    NoDatabase,
}

pub struct Bundles {
    store: Arc<dyn ObjectStore>,
    cache: Cache<String, Bytes>,
}

/// The object key of a bundle, when `database` is a canonical UUID and `sha256` 64 lowercase
/// hex; anything else names no bundle.
pub fn key(database: &str, sha256: &str) -> Option<(Uuid, String)> {
    let id = Uuid::parse_str(database)
        .ok()
        .filter(|id| id.hyphenated().to_string() == database)?;
    let hex = sha256.len() == 64
        && sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then(|| (id, format!("{database}/{sha256}")))
}

impl Bundles {
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Self {
            store,
            cache: Cache::builder()
                .weigher(|_k: &String, v: &Bytes| u32::try_from(v.len()).unwrap_or(u32::MAX))
                .max_capacity(CACHE_BYTES)
                .build(),
        }
    }

    /// The bundle's bytes; `None` when the store has none.
    pub async fn get(&self, key: &str) -> Result<Option<Bytes>, StorageError> {
        if let Some(hit) = self.cache.get(key).await {
            return Ok(Some(hit));
        }
        let object = match self.store.get(key, None).await {
            Ok(o) => o,
            Err(StorageError::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let body = chelabase_storage::collect(object.body).await?;
        self.cache.insert(key.to_owned(), body.clone()).await;
        Ok(Some(body))
    }

    /// Stores `body` under `key` when it hashes to `sha256` and the database is live. Always
    /// an overwrite: the key is content-addressed, and a rewrite refreshes the object's time,
    /// which the bundle sweep's grace counts from.
    pub async fn put(
        &self,
        directory: &dyn Directory,
        database: Uuid,
        sha256: &str,
        key: &str,
        body: Bytes,
    ) -> anyhow::Result<Put> {
        if hex::encode(Sha256::digest(&body)) != sha256 {
            return Ok(Put::Mismatch);
        }
        if !directory.database_live(database).await? {
            return Ok(Put::NoDatabase);
        }
        let opts = PutOptions {
            content_type: Some("application/json".into()),
            if_none_match: false,
        };
        self.store.put(key, body, opts).await?;
        Ok(Put::Stored)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_canonical() {
        let db = Uuid::new_v4().to_string();
        let sha = "0".repeat(64);
        assert_eq!(key(&db, &sha).unwrap().1, format!("{db}/{sha}"));
        for (d, s) in [
            (db.to_uppercase(), sha.clone()),
            (db.replace('-', ""), sha.clone()),
            ("../x".into(), sha.clone()),
            (db.clone(), "A".repeat(64)),
            (db.clone(), "0".repeat(63)),
            (db.clone(), format!("{}/", "0".repeat(63))),
        ] {
            assert!(key(&d, &s).is_none(), "{d} {s}");
        }
    }
}
