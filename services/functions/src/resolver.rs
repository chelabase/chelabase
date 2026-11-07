//! Resolutions, cached per `(branch, name)` for a TTL (30 s) and dropped for the whole branch
//! when `chelabase.fn.changed.<branch>` arrives, so a change applies within a second while NATS
//! is up and within the TTL when it isn't. Unknown functions are cached too (as `None`), so a
//! flood of bad names doesn't reach the database. Errors are never cached.

use std::sync::Arc;
use std::time::Duration;

use chelabase_nats::NatsConfig;
use chelabase_nats::fn_events::{CHANGED, FnChanged, changed_subject};
use futures_util::StreamExt;
use moka::future::Cache;
use uuid::Uuid;

use crate::directory::{Directory, Resolution};

pub const TTL: Duration = Duration::from_secs(30);
const RETRY: Duration = Duration::from_secs(2);

type Key = (Uuid, String);

pub struct Resolver {
    directory: Arc<dyn Directory>,
    public_url: String,
    cache: Cache<Key, Option<Arc<Resolution>>>,
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
        let load = async {
            let parts = self.directory.resolve(branch, name).await?;
            Ok::<_, anyhow::Error>(
                parts.map(|p| Arc::new(Resolution::build(p, branch, &self.public_url))),
            )
        };
        self.cache
            .try_get_with((branch, name.to_owned()), load)
            .await
            .map_err(|e| anyhow::anyhow!("resolve {name} on {branch}: {e}"))
    }

    /// Drops every cached resolution of `branch`.
    pub fn forget_branch(&self, branch: Uuid) {
        if let Err(e) = self
            .cache
            .invalidate_entries_if(move |(b, _), _| *b == branch)
        {
            tracing::warn!("couldn't drop branch {branch}'s resolutions ({e}); dropping all");
            self.cache.invalidate_all();
        }
    }

    pub fn forget_all(&self) {
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
            Some(branch) => resolver.forget_branch(branch),
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A directory whose answer the test sets, counting lookups.
    #[derive(Default)]
    struct Fake {
        enabled: Mutex<Option<bool>>,
        fail: Mutex<bool>,
        lookups: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Directory for Fake {
        async fn resolve(&self, _branch: Uuid, _name: &str) -> anyhow::Result<Option<Parts>> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(!*self.fail.lock().unwrap(), "metadata is down");
            Ok(self.enabled.lock().unwrap().map(|enabled| Parts {
                database_id: Uuid::nil(),
                function_id: Uuid::nil(),
                bundle_sha256: "a".repeat(64),
                auth: FunctionAuth::Key,
                enabled,
                secrets: Vec::new(),
                service_key: None,
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
