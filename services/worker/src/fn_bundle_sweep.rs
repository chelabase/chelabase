//! Edge functions m7: the bundle sweep. Function bundles live at `{database_id}/{sha256}` in
//! the `chelabase-functions` bucket; one nothing references any more is deleted once it is
//! older than [`GRACE`]. Hourly ([`EVERY`]), as the `functions` S3 identity:
//!
//! 1. The store's own clock, from a probe object (`_sweep/clock`), never the worker's.
//! 2. The **whole** bucket is listed, so a missed pass or a late upload never orphans a prefix.
//! 3. **After** listing, each listed database's references are read
//!    ([`chelabase_metadata::MetadataClient::referenced_bundles`], every deployment, active or
//!    kept for a rollback). An unknown or tombstoned database references nothing, so its whole
//!    prefix goes, including a bundle uploaded after its tombstone.
//! 4. An unreferenced object older than the grace is listed again just before its delete and
//!    kept if its time moved inside the grace: a deploy always overwrites its bundle (the
//!    functions service's `PUT`), so a redeploy of an old hash during the pass refreshes it
//!    before its deployment row exists.
//!
//! Keys that aren't `{uuid}/{64 hex}` are left alone. A database whose references can't be
//! read loses nothing this pass.
//!
//! **The remaining window.** The recheck and the delete are two calls: a redeploy of that very
//! hash landing between them (its `PUT`, then its deployment row) loses its bundle, and the
//! function fails to load until it is deployed again (which uploads the bundle again). The
//! delete can't be made conditional: [`ObjectStore`] has no conditional delete, S3's
//! `DeleteObject` takes no `If-Unmodified-Since`, and an `If-Match` on the ETag can't tell a
//! rewrite of the same bytes (a bundle's key is its hash) from the object listed. The window is
//! the gap between two requests, and only for a bundle no deployment references and nothing
//! has written for longer than [`GRACE`].

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use chelabase_metadata::MetadataClient;
use chelabase_storage::{DatedObject, ObjectStore, PutOptions};
use chelabase_types::DatabaseId;
use tracing::{info, warn};
use uuid::Uuid;

/// An unreferenced bundle is kept this long after its last write.
pub const GRACE: Duration = Duration::from_secs(3600);
/// How often a pass runs.
pub const EVERY: Duration = Duration::from_secs(3600);
/// The probe object whose time is the store's clock.
pub const CLOCK_KEY: &str = "_sweep/clock";

/// The bundle hashes a database's deployments still name: empty for an unknown or tombstoned
/// database.
#[async_trait::async_trait]
pub trait BundleRefs: Send + Sync {
    async fn referenced(&self, database: DatabaseId) -> Result<HashSet<String>, String>;
}

#[async_trait::async_trait]
impl BundleRefs for MetadataClient {
    async fn referenced(&self, database: DatabaseId) -> Result<HashSet<String>, String> {
        if !self
            .live_database(database)
            .await
            .map_err(|e| e.to_string())?
        {
            return Ok(HashSet::new());
        }
        Ok(self
            .referenced_bundles(database)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .collect())
    }
}

/// What a pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub deleted: usize,
    pub referenced: usize,
    /// Unreferenced, but inside the grace (by the listing or by the recheck).
    pub young: usize,
    /// Databases whose references couldn't be read (their bundles stay).
    pub unread: usize,
    /// Keys that aren't bundles.
    pub ignored: usize,
}

/// `{database_id}/{sha256}`: a canonical UUID and 64 lowercase hex.
fn bundle_key(key: &str) -> Option<(DatabaseId, &str)> {
    let (db, sha) = key.split_once('/')?;
    let id = Uuid::parse_str(db)
        .ok()
        .filter(|id| id.hyphenated().to_string() == db)?;
    let hex = sha.len() == 64 && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    hex.then_some((DatabaseId(id), sha))
}

fn age(now: SystemTime, then: SystemTime) -> Duration {
    now.duration_since(then).unwrap_or_default()
}

/// The object at exactly `key`, with its time.
async fn dated(store: &dyn ObjectStore, key: &str) -> Result<Option<DatedObject>, String> {
    Ok(store
        .list_dated(key)
        .await
        .map_err(|e| format!("list {key}: {e}"))?
        .into_iter()
        .find(|o| o.key == key))
}

/// The store's clock: the time it gives a fresh write of [`CLOCK_KEY`].
async fn store_now(store: &dyn ObjectStore) -> Result<SystemTime, String> {
    store
        .put(CLOCK_KEY, Bytes::new(), PutOptions::default())
        .await
        .map_err(|e| format!("write {CLOCK_KEY}: {e}"))?;
    dated(store, CLOCK_KEY)
        .await?
        .map(|o| o.modified)
        .ok_or_else(|| format!("{CLOCK_KEY} isn't listed after its write"))
}

/// One pass (the module docs' steps).
pub async fn sweep_bundles(
    store: &dyn ObjectStore,
    refs: &dyn BundleRefs,
    grace: Duration,
) -> Result<SweepReport, String> {
    let now = store_now(store).await?;
    let listed = store
        .list_dated("")
        .await
        .map_err(|e| format!("list the bundles: {e}"))?;
    let mut report = SweepReport::default();
    let mut by_db: BTreeMap<Uuid, Vec<(String, String, SystemTime)>> = BTreeMap::new();
    for o in listed {
        match bundle_key(&o.key) {
            Some((db, sha)) => {
                let sha = sha.to_owned();
                by_db
                    .entry(db.as_uuid())
                    .or_default()
                    .push((o.key, sha, o.modified));
            }
            None if o.key == CLOCK_KEY => {}
            None => report.ignored += 1,
        }
    }
    for (db, objects) in by_db {
        let db = DatabaseId(db);
        let referenced = match refs.referenced(db).await {
            Ok(r) => r,
            Err(e) => {
                warn!(database_id = %db, "bundle sweep: references unreadable, nothing deleted: {e}");
                report.unread += 1;
                continue;
            }
        };
        for (key, sha, modified) in objects {
            if referenced.contains(&sha) {
                report.referenced += 1;
                continue;
            }
            if age(now, modified) <= grace {
                report.young += 1;
                continue;
            }
            // A redeploy since the listing rewrote it: keep it.
            match dated(store, &key).await? {
                Some(o) if age(now, o.modified) > grace => {}
                Some(_) => {
                    report.young += 1;
                    continue;
                }
                None => continue,
            }
            store
                .delete(&key)
                .await
                .map_err(|e| format!("delete {key}: {e}"))?;
            report.deleted += 1;
        }
    }
    Ok(report)
}

/// The hourly loop.
pub async fn bundle_sweep_loop(store: std::sync::Arc<dyn ObjectStore>, metadata: MetadataClient) {
    let mut every = tokio::time::interval(EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        match sweep_bundles(store.as_ref(), &metadata, GRACE).await {
            Ok(r) => info!(
                deleted = r.deleted,
                referenced = r.referenced,
                young = r.young,
                unread = r.unread,
                ignored = r.ignored,
                "bundle sweep done"
            ),
            Err(e) => warn!("bundle sweep failed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_storage::MemoryStore;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    const HOUR: Duration = Duration::from_secs(3600);

    fn sha(c: char) -> String {
        c.to_string().repeat(64)
    }

    /// References by database; a database not in the map is unknown (none). `on_read` runs
    /// when a database's references are read (after the listing).
    #[derive(Default)]
    struct Refs {
        by_db: HashMap<DatabaseId, HashSet<String>>,
        broken: HashSet<DatabaseId>,
        #[allow(clippy::type_complexity)]
        on_read: Mutex<Option<Box<dyn FnMut(DatabaseId) + Send>>>,
    }

    #[async_trait::async_trait]
    impl BundleRefs for Refs {
        async fn referenced(&self, db: DatabaseId) -> Result<HashSet<String>, String> {
            if let Some(f) = self.on_read.lock().unwrap().as_mut() {
                f(db);
            }
            if self.broken.contains(&db) {
                return Err("metadata down".into());
            }
            Ok(self.by_db.get(&db).cloned().unwrap_or_default())
        }
    }

    struct World {
        store: Arc<MemoryStore>,
        t0: SystemTime,
    }

    impl World {
        fn new() -> World {
            let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
            let store = Arc::new(MemoryStore::default());
            store.set_now(t0);
            World { store, t0 }
        }

        /// A bundle written at `t0 + at`.
        async fn put(&self, db: DatabaseId, sha: &str, at: Duration) -> String {
            self.store.set_now(self.t0 + at);
            let key = format!("{db}/{sha}");
            self.store
                .put(&key, Bytes::from_static(b"{}"), PutOptions::default())
                .await
                .unwrap();
            key
        }

        fn at(&self, at: Duration) {
            self.store.set_now(self.t0 + at);
        }

        async fn keys(&self) -> HashSet<String> {
            self.store
                .list("")
                .await
                .unwrap()
                .into_iter()
                .map(|o| o.key)
                .filter(|k| k != CLOCK_KEY)
                .collect()
        }

        async fn sweep(&self, refs: &Refs) -> SweepReport {
            sweep_bundles(self.store.as_ref(), refs, GRACE)
                .await
                .unwrap()
        }
    }

    fn set(keys: &[&String]) -> HashSet<String> {
        keys.iter().map(|k| (*k).clone()).collect()
    }

    /// An unreferenced bundle older than the grace goes; a younger one stays, until it ages.
    #[tokio::test]
    async fn fn_bundle_sweep_keeps_young_bundles_for_the_grace() {
        let w = World::new();
        let db = DatabaseId::new();
        let old = w.put(db, &sha('a'), Duration::ZERO).await;
        let young = w.put(db, &sha('b'), HOUR).await;
        let mut refs = Refs::default();
        refs.by_db.insert(db, HashSet::new());
        w.at(HOUR + HOUR / 2);
        let r = w.sweep(&refs).await;
        assert_eq!((r.deleted, r.young), (1, 1), "{r:?}");
        assert_eq!(w.keys().await, set(&[&young]));
        assert!(!w.keys().await.contains(&old));
        w.at(2 * HOUR + Duration::from_secs(1));
        w.sweep(&refs).await;
        assert!(w.keys().await.is_empty());
    }

    /// Every bundle a deployment names stays, however old: the active one and one kept for a
    /// rollback alike.
    #[tokio::test]
    async fn fn_bundle_sweep_keeps_rollback_deployments() {
        let w = World::new();
        let db = DatabaseId::new();
        let active = w.put(db, &sha('a'), Duration::ZERO).await;
        let rollback = w.put(db, &sha('b'), Duration::ZERO).await;
        w.put(db, &sha('c'), Duration::ZERO).await;
        let mut refs = Refs::default();
        refs.by_db.insert(db, [sha('a'), sha('b')].into());
        w.at(10 * HOUR);
        let r = w.sweep(&refs).await;
        assert_eq!((r.deleted, r.referenced), (1, 2), "{r:?}");
        assert_eq!(w.keys().await, set(&[&active, &rollback]));
    }

    /// A deleted (or unknown) database loses its whole prefix; another database's bundle of the
    /// same hash stays. A bundle uploaded after the tombstone goes on a later pass, once past
    /// the grace.
    #[tokio::test]
    async fn fn_bundle_sweep_removes_a_deleted_databases_prefix() {
        let w = World::new();
        let (gone, live) = (DatabaseId::new(), DatabaseId::new());
        w.put(gone, &sha('a'), Duration::ZERO).await;
        w.put(gone, &sha('b'), Duration::ZERO).await;
        let kept = w.put(live, &sha('a'), Duration::ZERO).await;
        let mut refs = Refs::default();
        refs.by_db.insert(live, [sha('a')].into());
        w.at(2 * HOUR);
        let r = w.sweep(&refs).await;
        assert_eq!(r.deleted, 2, "{r:?}");
        assert_eq!(w.keys().await, set(&[&kept]));

        let late = w.put(gone, &sha('c'), 3 * HOUR).await;
        w.at(3 * HOUR + HOUR / 2);
        w.sweep(&refs).await;
        assert!(w.keys().await.contains(&late), "inside the grace");
        w.at(5 * HOUR);
        w.sweep(&refs).await;
        assert_eq!(w.keys().await, set(&[&kept]));
    }

    /// An old unreferenced hash redeployed after the listing (its bundle rewritten before its
    /// deployment row exists) survives: the recheck sees the new time.
    #[tokio::test]
    async fn fn_bundle_sweep_keeps_a_bundle_redeployed_during_the_pass() {
        let w = World::new();
        let db = DatabaseId::new();
        let key = w.put(db, &sha('a'), Duration::ZERO).await;
        let mut refs = Refs::default();
        refs.by_db.insert(db, HashSet::new());
        w.at(5 * HOUR);
        let store = w.store.clone();
        let rewrite = key.clone();
        *refs.on_read.lock().unwrap() = Some(Box::new(move |_| {
            // The functions service's PUT: an overwrite, at the store's current time.
            futures_util::FutureExt::now_or_never(store.put(
                &rewrite,
                Bytes::from_static(b"{}"),
                PutOptions::default(),
            ))
            .unwrap()
            .unwrap();
        }));
        let r = w.sweep(&refs).await;
        assert_eq!((r.deleted, r.young), (0, 1), "{r:?}");
        assert_eq!(w.keys().await, set(&[&key]));
    }

    /// Keys that aren't bundles stay, and a database whose references can't be read loses
    /// nothing while the others are swept.
    #[tokio::test]
    async fn fn_bundle_sweep_leaves_other_keys_and_unreadable_databases() {
        let w = World::new();
        let (broken, fine) = (DatabaseId::new(), DatabaseId::new());
        let kept = w.put(broken, &sha('a'), Duration::ZERO).await;
        w.put(fine, &sha('a'), Duration::ZERO).await;
        for other in [
            "notes.txt",
            "not-a-uuid/aaaa",
            &format!("{fine}/{}", "A".repeat(64)),
        ] {
            w.store
                .put(other, Bytes::new(), PutOptions::default())
                .await
                .unwrap();
        }
        let mut refs = Refs::default();
        refs.broken.insert(broken);
        refs.by_db.insert(fine, HashSet::new());
        w.at(2 * HOUR);
        let r = w.sweep(&refs).await;
        assert_eq!((r.deleted, r.unread, r.ignored), (1, 1, 3), "{r:?}");
        let keys = w.keys().await;
        assert!(keys.contains(&kept));
        assert_eq!(keys.len(), 4);
    }
}
