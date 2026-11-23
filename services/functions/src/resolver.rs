//! Resolutions, cached per `(branch, name)` for a TTL (30 s) and dropped for the whole branch
//! when `chelabase.fn.changed.<branch>` arrives, so a change applies within a second while NATS
//! is up and within the TTL when it isn't. Errors are never cached.
//!
//! A name the function-name grammar refuses can't exist, so it's answered as unknown before any
//! cache or metadata lookup. Unknown well-formed names are remembered too, so a flood of bad
//! names doesn't reach the database, but in a separate miss cache: per branch, at most
//! [`MISSES_PER_BRANCH`] names for a hard [`MISS_TTL`] (5 s), across at most [`MISS_BRANCHES`]
//! branches. Misses never evict resolved functions, and a function created without a change
//! event resolves within [`MISS_TTL`].
//!
//! A change can arrive while a lookup is reading the old row (or finding none). moka's
//! invalidation only drops entries inserted before it, so each branch has a generation that
//! [`Resolver::forget_branch`] bumps (and [`Resolver::forget_all`] bumps for every branch). A
//! lookup notes the generation before it loads; when it moved by the time the entry (hit or
//! miss) is cached, the entry is dropped and the answer is loaded again, uncached.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chelabase_nats::NatsConfig;
use chelabase_nats::fn_events::{CHANGED, FnChanged, changed_subject};
use futures_util::StreamExt;
use moka::future::Cache;
use uuid::Uuid;

use crate::directory::{Directory, Resolution};

/// The branches whose misses are remembered at once (least recently used dropped first).
pub const MISS_BRANCHES: u64 = 1_024;
/// The unknown names remembered per branch.
pub const MISSES_PER_BRANCH: u64 = 256;

pub const TTL: Duration = Duration::from_secs(30);
/// How long a miss is remembered when no change event drops it.
pub const MISS_TTL: Duration = Duration::from_secs(5);
const RETRY: Duration = Duration::from_secs(2);

type Key = (Uuid, String);

/// One branch's remembered misses.
type Misses = moka::sync::Cache<String, ()>;

/// Why a load put nothing in the hit cache.
enum Unresolved {
    Missing,
    Failed(anyhow::Error),
}

/// Branch generations are striped: two branches sharing a stripe only cost an extra lookup.
const STRIPES: usize = 1024;

pub struct Resolver {
    directory: Arc<dyn Directory>,
    public_url: String,
    /// Resolved functions only.
    cache: Cache<Key, Arc<Resolution>>,
    /// Unknown names, per branch (see the module doc).
    misses: moka::sync::Cache<Uuid, Arc<Misses>>,
    /// Bumped by `forget_all`.
    epoch: AtomicU64,
    /// Bumped by `forget_branch` for the branch's stripe.
    generations: Box<[AtomicU64]>,
    /// m11: notified on every change event (the container keeper's wake).
    changes: Arc<tokio::sync::Notify>,
    miss_ttl: Duration,
}

impl Resolver {
    pub fn new(directory: Arc<dyn Directory>, public_url: String, ttl: Duration) -> Self {
        Self {
            directory,
            public_url,
            cache: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(ttl)
                .support_invalidation_closures()
                .build(),
            misses: moka::sync::Cache::builder()
                .max_capacity(MISS_BRANCHES)
                .build(),
            epoch: AtomicU64::new(0),
            generations: (0..STRIPES).map(|_| AtomicU64::new(0)).collect(),
            changes: Arc::default(),
            miss_ttl: MISS_TTL,
        }
    }

    /// How long a miss is remembered ([`MISS_TTL`] by default).
    pub fn with_miss_ttl(mut self, ttl: Duration) -> Self {
        self.miss_ttl = ttl;
        self
    }

    /// m11: what each change event notifies.
    pub fn with_changes(mut self, changes: Arc<tokio::sync::Notify>) -> Self {
        self.changes = changes;
        self
    }

    fn stripe(&self, branch: Uuid) -> &AtomicU64 {
        let n = branch.as_u128();
        &self.generations[((n ^ (n >> 64)) as usize) % STRIPES]
    }

    fn generation(&self, branch: Uuid) -> (u64, u64) {
        (
            self.epoch.load(Ordering::SeqCst),
            self.stripe(branch).load(Ordering::SeqCst),
        )
    }

    #[cfg(test)]
    fn miss_count(&self) -> u64 {
        self.misses.run_pending_tasks();
        self.misses
            .iter()
            .map(|(_, m)| {
                m.run_pending_tasks();
                m.entry_count()
            })
            .sum()
    }

    fn cached_miss(&self, branch: Uuid, name: &str) -> bool {
        self.misses
            .get(&branch)
            .is_some_and(|m| m.contains_key(name))
    }

    /// Remembers `name` as unknown on `branch` (until [`MISS_TTL`] from now, not refreshed by
    /// later hits: `contains_key` doesn't touch it, and it's only inserted after a lookup).
    fn remember_miss(&self, branch: Uuid, name: &str) {
        let ttl = self.miss_ttl;
        self.misses
            .get_with(branch, || {
                Arc::new(
                    Misses::builder()
                        .max_capacity(MISSES_PER_BRANCH)
                        .time_to_live(ttl)
                        .build(),
                )
            })
            .insert(name.to_owned(), ());
    }

    fn forget_miss(&self, branch: Uuid, name: &str) {
        if let Some(m) = self.misses.get(&branch) {
            m.invalidate(name);
        }
    }

    pub fn directory(&self) -> &Arc<dyn Directory> {
        &self.directory
    }

    pub async fn resolve(
        &self,
        branch: Uuid,
        name: &str,
    ) -> anyhow::Result<Option<Arc<Resolution>>> {
        // Every function and service name passed this at create time (and the schema's CHECK).
        if !chelabase_metadata::valid_function_name(name) {
            return Ok(None);
        }
        let key = (branch, name.to_owned());
        // Hits first: a stale miss cached late by a raced lookup never shadows a live hit.
        if let Some(found) = self.cache.get(&key).await {
            return Ok(Some(found));
        }
        if self.cached_miss(branch, name) {
            return Ok(None);
        }
        let before = self.generation(branch);
        let loaded = self
            .cache
            .try_get_with(key.clone(), async {
                match self.load(branch, name).await {
                    Ok(Some(found)) => Ok(found),
                    Ok(None) => Err(Unresolved::Missing),
                    Err(e) => Err(Unresolved::Failed(e)),
                }
            })
            .await;
        let found = match loaded {
            Ok(found) => Some(found),
            Err(e) => match &*e {
                Unresolved::Missing => {
                    self.remember_miss(branch, name);
                    None
                }
                Unresolved::Failed(e) => {
                    anyhow::bail!("resolve {name} on {branch}: {e:#}")
                }
            },
        };
        if self.generation(branch) == before {
            return Ok(found);
        }
        // A change arrived while this was loading: what it read may be the old row (or the lack
        // of one).
        self.cache.invalidate(&key).await;
        self.forget_miss(branch, name);
        self.load(branch, name)
            .await
            .map_err(|e| anyhow::anyhow!("resolve {name} on {branch}: {e}"))
    }

    async fn load(&self, branch: Uuid, name: &str) -> anyhow::Result<Option<Arc<Resolution>>> {
        let parts = self.directory.resolve(branch, name).await?;
        Ok(parts.map(|p| Arc::new(Resolution::build(p, branch, &self.public_url))))
    }

    /// Drops every cached resolution and miss of `branch`, and any a lookup in flight would cache.
    pub fn forget_branch(&self, branch: Uuid) {
        self.stripe(branch).fetch_add(1, Ordering::SeqCst);
        self.misses.invalidate(&branch);
        if let Err(e) = self
            .cache
            .invalidate_entries_if(move |(b, _), _| *b == branch)
        {
            tracing::warn!("couldn't drop branch {branch}'s resolutions ({e}); dropping all");
            self.cache.invalidate_all();
        }
    }

    pub fn forget_all(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.misses.invalidate_all();
        self.cache.invalidate_all();
    }
}

/// The branch a `chelabase.fn.changed.<branch>` message names: its payload's, when the subject
/// is that branch's.
pub fn changed_branch(subject: &str, payload: &[u8]) -> Option<Uuid> {
    let event: FnChanged = serde_json::from_slice(payload).ok()?;
    (changed_subject(event.branch_id) == subject).then_some(event.branch_id)
}

/// Listens for `chelabase.fn.changed.*` in the background, reconnecting forever. Each new
/// subscription first drops the whole cache: events sent while it was away are lost.
pub fn spawn_listener(resolver: Arc<Resolver>, config: NatsConfig) {
    tokio::spawn(async move {
        loop {
            match listen(&resolver, &config).await {
                Ok(()) => {
                    tracing::warn!("the functions change subscription ended; subscribing again")
                }
                Err(e) => tracing::warn!(
                    "NATS change events unavailable (the {}s TTL applies): {e}",
                    TTL.as_secs()
                ),
            }
            tokio::time::sleep(RETRY).await;
        }
    });
}

async fn listen(resolver: &Resolver, config: &NatsConfig) -> anyhow::Result<()> {
    let client = chelabase_nats::connect(config).await?;
    let mut sub = client.subscribe(format!("{CHANGED}.*")).await?;
    resolver.forget_all();
    tracing::info!("listening for functions change events");
    while let Some(msg) = sub.next().await {
        match changed_branch(msg.subject.as_str(), &msg.payload) {
            Some(branch) => {
                resolver.forget_branch(branch);
                resolver.changes.notify_one();
            }
            None => tracing::debug!(subject = %msg.subject, "ignoring an unreadable change event"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::directory::Parts;
    use chelabase_metadata::FunctionAuth;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// A directory whose answer the test sets, counting lookups. With `known` set, only those
    /// names exist (and their lookups are also counted in `known_lookups`).
    #[derive(Default)]
    struct Fake {
        enabled: Mutex<Option<bool>>,
        fail: Mutex<bool>,
        lookups: AtomicUsize,
        known: Mutex<Option<std::collections::HashSet<String>>>,
        known_lookups: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Directory for Fake {
        async fn resolve(&self, _branch: Uuid, name: &str) -> anyhow::Result<Option<Parts>> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!*self.fail.lock().unwrap(), "metadata is down");
            if let Some(known) = &*self.known.lock().unwrap() {
                if !known.contains(name) {
                    return Ok(None);
                }
                self.known_lookups.fetch_add(1, Ordering::SeqCst);
            }
            Ok(self.enabled.lock().unwrap().map(|enabled| Parts {
                database_id: Uuid::nil(),
                function_id: Uuid::nil(),
                bundle_sha256: "a".repeat(64),
                auth: FunctionAuth::Key,
                enabled,
                secrets: Vec::new(),
                service_key: None,
                service: None,
            }))
        }
        async fn record_error(&self, _: Uuid, _: Uuid, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn database_live(&self, _: Uuid) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    fn resolver(fake: &Arc<Fake>, ttl: Duration) -> Resolver {
        Resolver::new(fake.clone(), "u".into(), ttl)
    }

    #[tokio::test]
    async fn cached_until_the_branch_changes() {
        let fake = Arc::new(Fake::default());
        *fake.enabled.lock().unwrap() = Some(true);
        let r = resolver(&fake, TTL);
        let (b, other) = (Uuid::new_v4(), Uuid::new_v4());
        assert!(r.resolve(b, "hello").await.unwrap().unwrap().enabled);
        r.resolve(other, "hello").await.unwrap();
        *fake.enabled.lock().unwrap() = Some(false);
        assert!(
            r.resolve(b, "hello").await.unwrap().unwrap().enabled,
            "cached"
        );
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 2);
        r.forget_branch(b);
        assert!(
            !r.resolve(b, "hello").await.unwrap().unwrap().enabled,
            "dropped on change"
        );
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 3);
        r.resolve(other, "hello").await.unwrap();
        assert_eq!(
            fake.lookups.load(Ordering::SeqCst),
            3,
            "another branch keeps its entry"
        );
    }

    #[tokio::test]
    async fn unknown_is_cached_and_errors_are_not() {
        let fake = Arc::new(Fake::default());
        let r = resolver(&fake, TTL);
        let b = Uuid::new_v4();
        assert!(r.resolve(b, "nope").await.unwrap().is_none());
        assert!(r.resolve(b, "nope").await.unwrap().is_none());
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
        *fake.fail.lock().unwrap() = true;
        assert!(r.resolve(b, "down").await.is_err());
        *fake.fail.lock().unwrap() = false;
        *fake.enabled.lock().unwrap() = Some(true);
        assert!(
            r.resolve(b, "down").await.unwrap().is_some(),
            "the failure wasn't cached"
        );
    }

    /// AC 1: a name the function-name grammar refuses is answered as unknown with no lookup
    /// and no cache entry, on either cache.
    #[tokio::test]
    async fn an_invalid_name_never_reaches_the_cache() {
        let fake = Arc::new(Fake::default());
        *fake.enabled.lock().unwrap() = Some(true);
        let r = resolver(&fake, TTL);
        let b = Uuid::new_v4();
        let too_long = format!("a{}", "b".repeat(63));
        for name in [
            "",
            "Hello",
            "1abc",
            "-abc",
            "a_b",
            "a/b",
            "a b",
            "h\u{e9}llo",
            "hello\0",
            too_long.as_str(),
        ] {
            assert!(
                r.resolve(b, name).await.unwrap().is_none(),
                "{name:?} resolved"
            );
            assert!(
                !r.cache.contains_key(&(b, name.to_owned())),
                "{name:?} cached"
            );
        }
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 0, "metadata was asked");
        r.cache.run_pending_tasks().await;
        assert_eq!(r.cache.entry_count(), 0);
        assert_eq!(r.miss_count(), 0, "a miss was cached");
        // The grammar's edges still resolve.
        let longest = format!("a{}", "-9".repeat(31));
        assert_eq!(longest.len(), 63);
        assert!(r.resolve(b, &longest).await.unwrap().is_some());
        assert!(r.resolve(b, "a").await.unwrap().is_some());
    }

    /// AC 2: a flood of distinct unknown (but well-formed) names on one branch never evicts the
    /// resolved functions; they're all still served from the cache.
    #[tokio::test]
    async fn misses_do_not_evict_resolved_functions() {
        let fake = Arc::new(Fake::default());
        *fake.enabled.lock().unwrap() = Some(true);
        let real: Vec<String> = (0..64).map(|i| format!("real-{i}")).collect();
        *fake.known.lock().unwrap() = Some(real.iter().cloned().collect());
        let r = resolver(&fake, TTL);
        let b = Uuid::new_v4();
        for name in &real {
            assert!(r.resolve(b, name).await.unwrap().is_some());
        }
        assert_eq!(fake.known_lookups.load(Ordering::SeqCst), real.len());
        // Each unknown name is asked for a few times, so a frequency-based admission policy
        // admits it as it would a popular name.
        for i in 0..20_000 {
            let name = format!("nope-{i}");
            for _ in 0..3 {
                assert!(r.resolve(b, &name).await.unwrap().is_none());
            }
        }
        r.cache.run_pending_tasks().await;
        for name in &real {
            assert!(r.resolve(b, name).await.unwrap().is_some());
        }
        assert_eq!(
            fake.known_lookups.load(Ordering::SeqCst),
            real.len(),
            "a resolved function was evicted by misses"
        );
    }

    /// AC 3: a miss is remembered for at most the miss TTL (hard, from the first miss: asking
    /// again doesn't extend it), so a function created afterwards resolves once it passes even
    /// with no change event. The default is a few seconds, far below the hit TTL.
    #[tokio::test]
    async fn a_cached_miss_expires() {
        assert!(MISS_TTL <= Duration::from_secs(5) && MISS_TTL < TTL);
        let fake = Arc::new(Fake::default());
        assert_eq!(resolver(&fake, TTL).miss_ttl, MISS_TTL, "the default");
        let miss_ttl = Duration::from_millis(400);
        let r = resolver(&fake, TTL).with_miss_ttl(miss_ttl);
        let b = Uuid::new_v4();
        let first = std::time::Instant::now();
        assert!(r.resolve(b, "later").await.unwrap().is_none());
        *fake.enabled.lock().unwrap() = Some(true); // created; no change event
        assert!(
            r.resolve(b, "later").await.unwrap().is_none(),
            "the miss is cached"
        );
        assert_eq!(fake.lookups.load(Ordering::SeqCst), 1);
        // Keep asking until the TTL has passed since the first miss.
        while first.elapsed() < miss_ttl + Duration::from_millis(100) {
            r.resolve(b, "later").await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            r.resolve(b, "later").await.unwrap().is_some(),
            "the stale miss outlived its TTL"
        );
    }

    /// With change events up, a created function resolves at once: the event drops the miss.
    #[tokio::test]
    async fn a_change_event_drops_cached_misses() {
        for forget_all in [false, true] {
            let fake = Arc::new(Fake::default());
            let r = resolver(&fake, TTL);
            let (b, other) = (Uuid::new_v4(), Uuid::new_v4());
            assert!(r.resolve(b, "later").await.unwrap().is_none());
            assert!(r.resolve(other, "later").await.unwrap().is_none());
            *fake.enabled.lock().unwrap() = Some(true);
            if forget_all {
                r.forget_all();
            } else {
                r.forget_branch(b);
            }
            assert!(r.resolve(b, "later").await.unwrap().is_some());
            let lookups = fake.lookups.load(Ordering::SeqCst);
            let cached = r.resolve(other, "later").await.unwrap();
            if forget_all {
                assert!(cached.is_some());
            } else {
                assert!(cached.is_none(), "another branch keeps its miss");
                assert_eq!(fake.lookups.load(Ordering::SeqCst), lookups);
            }
        }
    }

    /// NATS down: nothing invalidates, and a change still applies once the TTL runs out.
    #[tokio::test]
    async fn without_events_the_ttl_applies() {
        let fake = Arc::new(Fake::default());
        *fake.enabled.lock().unwrap() = Some(true);
        let r = resolver(&fake, Duration::from_millis(200));
        let b = Uuid::new_v4();
        assert!(r.resolve(b, "hello").await.unwrap().unwrap().enabled);
        *fake.enabled.lock().unwrap() = Some(false);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!r.resolve(b, "hello").await.unwrap().unwrap().enabled);
    }

    /// A directory that holds a lookup between reading the row and returning it.
    #[derive(Default)]
    struct Gated {
        enabled: std::sync::atomic::AtomicBool,
        missing: std::sync::atomic::AtomicBool,
        gate: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl Directory for Gated {
        async fn resolve(&self, _branch: Uuid, _name: &str) -> anyhow::Result<Option<Parts>> {
            let enabled = self.enabled.load(Ordering::SeqCst);
            let missing = self.missing.load(Ordering::SeqCst);
            if self.gate.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if missing {
                return Ok(None);
            }
            Ok(Some(Parts {
                database_id: Uuid::nil(),
                function_id: Uuid::nil(),
                bundle_sha256: "a".repeat(64),
                auth: FunctionAuth::Key,
                enabled,
                secrets: Vec::new(),
                service_key: None,
                service: None,
            }))
        }
        async fn record_error(&self, _: Uuid, _: Uuid, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn database_live(&self, _: Uuid) -> anyhow::Result<bool> {
            Ok(true)
        }
    }

    /// A lookup that read the old row before a change event arrived must not cache it: the
    /// next call sees the change, not the stale state for the whole TTL.
    #[tokio::test]
    async fn a_change_during_a_lookup_is_not_lost() {
        for forget_all in [false, true] {
            let gate = Arc::new(Gated::default());
            gate.enabled.store(true, Ordering::SeqCst);
            gate.gate.store(true, Ordering::SeqCst);
            let r = Arc::new(Resolver::new(gate.clone(), "u".into(), TTL));
            let b = Uuid::new_v4();
            let r2 = r.clone();
            let inflight = tokio::spawn(async move { r2.resolve(b, "hello").await.unwrap() });
            gate.entered.notified().await;
            gate.enabled.store(false, Ordering::SeqCst); // the disable commits
            if forget_all {
                r.forget_all(); // a resubscription
            } else {
                r.forget_branch(b); // the change event arrives
            }
            gate.release.notify_one();
            inflight.await.unwrap();
            assert!(
                !r.resolve(b, "hello").await.unwrap().unwrap().enabled,
                "the disabled function is still served from the cache (forget_all: {forget_all})"
            );
        }
    }

    /// The same for a miss: a lookup that found nothing before the function was created must
    /// not cache the miss past the change event.
    #[tokio::test]
    async fn a_creation_during_a_lookup_is_not_lost() {
        for forget_all in [false, true] {
            let gate = Arc::new(Gated::default());
            gate.enabled.store(true, Ordering::SeqCst);
            gate.missing.store(true, Ordering::SeqCst);
            gate.gate.store(true, Ordering::SeqCst);
            let r = Arc::new(Resolver::new(gate.clone(), "u".into(), TTL));
            let b = Uuid::new_v4();
            let r2 = r.clone();
            let inflight = tokio::spawn(async move { r2.resolve(b, "hello").await.unwrap() });
            gate.entered.notified().await;
            gate.missing.store(false, Ordering::SeqCst); // the create commits
            if forget_all {
                r.forget_all();
            } else {
                r.forget_branch(b);
            }
            gate.release.notify_one();
            inflight.await.unwrap();
            assert!(
                r.resolve(b, "hello").await.unwrap().is_some(),
                "the created function is still a cached miss (forget_all: {forget_all})"
            );
        }
    }

    #[test]
    fn change_events_name_their_branch() {
        let b = Uuid::new_v4();
        let payload = serde_json::to_vec(&FnChanged {
            branch_id: b,
            function: "hello".into(),
        })
        .unwrap();
        assert_eq!(changed_branch(&changed_subject(b), &payload), Some(b));
        assert_eq!(
            changed_branch(&changed_subject(Uuid::new_v4()), &payload),
            None,
            "subject and payload disagree"
        );
        assert_eq!(changed_branch(&changed_subject(b), b"{}"), None);
    }
}
