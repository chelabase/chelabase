//! A cache whose forgets win over a lookup in flight (S1, object-storage m9's audit).
//!
//! A NATS event's forget can land between a miss's lookup and its insert; a plain cache would
//! then keep the answer from before the change for its whole lifetime. Every forget bumps an
//! epoch before it drops entries, and a lookup's answer is kept only if the epoch it read
//! before the lookup is still current after the insert (else the insert is undone). A forget
//! of one entry makes the other lookups in flight skip caching once; they ask again next time.

use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub struct Fenced<K, V> {
    cache: moka::future::Cache<K, V>,
    epoch: AtomicU64,
}

impl<K, V> Fenced<K, V>
where
    K: Hash + Eq + Send + Sync + Clone + 'static,
    V: Send + Sync + Clone + 'static,
{
    pub fn new(max_capacity: u64, time_to_live: Duration) -> Self {
        Fenced {
            cache: moka::future::Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(time_to_live)
                .support_invalidation_closures()
                .build(),
            epoch: AtomicU64::new(0),
        }
    }

    /// The cached value, or `lookup`'s, kept unless a forget landed meanwhile.
    pub async fn get_or_lookup<E>(
        &self,
        key: K,
        lookup: impl Future<Output = Result<V, E>>,
    ) -> Result<V, E> {
        if let Some(hit) = self.cache.get(&key).await {
            return Ok(hit);
        }
        let before = self.epoch.load(Ordering::SeqCst);
        let value = lookup.await?;
        self.cache.insert(key.clone(), value.clone()).await;
        // A forget bumps the epoch before it drops entries: either it drops this insert, or
        // its bump is seen here and the insert is undone.
        if self.epoch.load(Ordering::SeqCst) != before {
            self.cache.invalidate(&key).await;
        }
        Ok(value)
    }

    /// Forgets the entries `pred` picks (every entry when the cache can't run the closure).
    pub fn forget_if(&self, pred: impl Fn(&K, &V) -> bool + Send + Sync + 'static) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        if let Err(e) = self.cache.invalidate_entries_if(pred) {
            tracing::warn!("dropping cached entries: {e}");
            self.cache.invalidate_all();
        }
    }

    pub fn forget_all(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.cache.invalidate_all();
    }

    #[cfg(test)]
    pub async fn insert(&self, key: K, value: V) {
        self.cache.insert(key, value).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A forget during a lookup (of that key or another) leaves nothing cached; one before
    /// the lookup doesn't stop it caching.
    #[tokio::test]
    async fn a_forget_during_a_lookup_wins() {
        let cache: Fenced<u32, u32> = Fenced::new(10, Duration::from_secs(60));
        cache.forget_all();
        let v = cache
            .get_or_lookup(1, async { Ok::<_, ()>(10) })
            .await
            .unwrap();
        assert_eq!(v, 10);
        assert_eq!(cache.cache.get(&1).await, Some(10), "cached");
        for during in [0, 1] {
            let v = cache
                .get_or_lookup(2, async {
                    if during == 0 {
                        cache.forget_if(|k, _| *k == 2);
                    } else {
                        cache.forget_all();
                    }
                    Ok::<_, ()>(20)
                })
                .await
                .unwrap();
            assert_eq!(v, 20, "the caller still gets its answer");
            assert_eq!(cache.cache.get(&2).await, None, "not cached ({during})");
        }
    }
}
