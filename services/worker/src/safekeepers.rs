//! The safekeepers' commit LSN, read directly from them (D11): a branch's head is the
//! larger of the pageserver's `last_record_lsn` and what the safekeepers have committed.
//!
//! The pageserver's own view lags after a tenant reload, and a branch at an LSN it hasn't
//! ingested yet makes it wait (up to its 300 s default, longer than the worker's own 120 s
//! create timeout, so a timeout shows up as a transient client-side error, not a 406; a 406
//! is a real refusal). `commit_lsn` is quorum-committed on any
//! safekeeper that reports it, so the maximum over any subset of them is safe.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::Mutex;

use chelabase_neon::safekeeper::{SafekeeperClient, SafekeeperError};
use chelabase_neon::storcon::head_lsn;
use chelabase_neon::{Lsn, NeonId};
use futures_util::future::join_all;

/// Where a timeline's commit LSN comes from: one safekeeper.
pub trait CommitLsnSource: Send + Sync {
    /// `None` when the safekeeper doesn't know the timeline.
    fn commit_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<Option<Lsn>, SafekeeperError>> + Send;
}

impl CommitLsnSource for SafekeeperClient {
    async fn commit_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<Lsn>, SafekeeperError> {
        SafekeeperClient::commit_lsn(self, tenant, timeline).await
    }
}

/// The safekeepers the worker can reach, by the id the storage controller knows them by.
pub struct SafekeeperSet<C> {
    clients: BTreeMap<u64, C>,
    /// Ids already logged as having no URL, so each is warned about once.
    warned_unmapped: Mutex<BTreeSet<u64>>,
}

impl<C: CommitLsnSource> SafekeeperSet<C> {
    pub fn new(clients: BTreeMap<u64, C>) -> Self {
        Self {
            clients,
            warned_unmapped: Mutex::default(),
        }
    }

    /// The timeline's head: `last_record` or the highest commit LSN any of the `active`
    /// safekeepers reports, whichever is ahead. Asks them all in parallel. When none
    /// answers (errors, 404s, nothing mapped) the head is `last_record`, logged at warn.
    pub async fn head(
        &self,
        last_record: Lsn,
        active: &[u64],
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Lsn {
        let mut asked = Vec::new();
        for id in active {
            match self.clients.get(id) {
                Some(client) => asked.push((*id, client)),
                None => {
                    if self.warned_unmapped.lock().unwrap().insert(*id) {
                        tracing::warn!(
                            safekeeper = id,
                            "no URL for the safekeeper in CHELABASE_SAFEKEEPER_HTTP; skipping it"
                        );
                    }
                }
            }
        }
        let answers =
            join_all(asked.iter().map(|(id, client)| async move {
                (*id, client.commit_lsn(tenant, timeline).await)
            }))
            .await;
        let mut commit = None;
        let mut errored = false;
        for (id, answer) in answers {
            match answer {
                Ok(Some(lsn)) => commit = commit.max(Some(lsn)),
                Ok(None) => {}
                Err(e) => {
                    errored = true;
                    tracing::warn!(safekeeper = id, %tenant, %timeline, "reading the commit LSN: {e:#}")
                }
            }
        }
        if commit.is_none() {
            // All 404s (the safekeepers don't know the timeline) is normal; an error isn't.
            if errored {
                tracing::warn!(
                    %tenant, %timeline,
                    "no safekeeper reported a commit LSN; the head is the pageserver's last record LSN"
                );
            } else {
                tracing::debug!(
                    %tenant, %timeline,
                    "the safekeepers don't know the timeline; the head is the pageserver's last record LSN"
                );
            }
        }
        head_lsn(last_record, commit)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use chelabase_neon::safekeeper::SafekeeperError;
    use chelabase_neon::{Lsn, NeonId};

    use super::*;

    /// Answers per safekeeper: `Ok(Some)`, `Ok(None)` (404) or an error.
    struct FakeSafekeeper {
        answer: Result<Option<Lsn>, &'static str>,
        asked: Mutex<u32>,
    }

    fn fake(answer: Result<Option<Lsn>, &'static str>) -> FakeSafekeeper {
        FakeSafekeeper {
            answer,
            asked: Mutex::new(0),
        }
    }

    impl CommitLsnSource for FakeSafekeeper {
        async fn commit_lsn(
            &self,
            _tenant: &NeonId,
            _timeline: &NeonId,
        ) -> Result<Option<Lsn>, SafekeeperError> {
            *self.asked.lock().unwrap() += 1;
            self.answer
                .map_err(|e| SafekeeperError::from(anyhow::anyhow!(e)))
        }
    }

    fn id(n: u128) -> NeonId {
        NeonId::from(uuid::Uuid::from_u128(n))
    }

    fn set(
        clients: impl IntoIterator<Item = (u64, FakeSafekeeper)>,
    ) -> SafekeeperSet<FakeSafekeeper> {
        SafekeeperSet::new(clients.into_iter().collect::<BTreeMap<_, _>>())
    }

    #[tokio::test]
    async fn head_uses_max_safekeeper_commit() {
        let s = set([
            (1, fake(Ok(Some(Lsn(0x300))))),
            (2, fake(Ok(Some(Lsn(0x500))))),
            (3, fake(Ok(None))),
            (4, fake(Err("down"))),
        ]);
        let head = s.head(Lsn(0x100), &[1, 2, 3, 4], &id(1), &id(2)).await;
        assert_eq!(head, Lsn(0x500));
        // A commit LSN behind the pageserver's last record doesn't pull the head back.
        let head = s.head(Lsn(0x900), &[1, 2], &id(1), &id(2)).await;
        assert_eq!(head, Lsn(0x900));
    }

    #[tokio::test]
    async fn head_falls_back_when_no_safekeeper_answers() {
        let s = set([(1, fake(Ok(None))), (2, fake(Err("down")))]);
        assert_eq!(
            s.head(Lsn(0x100), &[1, 2], &id(1), &id(2)).await,
            Lsn(0x100)
        );
        // No active safekeeper at all.
        assert_eq!(s.head(Lsn(0x100), &[], &id(1), &id(2)).await, Lsn(0x100));
    }

    #[tokio::test]
    async fn unmapped_safekeeper_is_skipped() {
        let s = set([(1, fake(Ok(Some(Lsn(0x300)))))]);
        // Safekeeper 7 has no URL: skipped (and logged once), the mapped one still answers.
        assert_eq!(
            s.head(Lsn(0x100), &[7, 1], &id(1), &id(2)).await,
            Lsn(0x300)
        );
        assert_eq!(s.head(Lsn(0x100), &[7], &id(1), &id(2)).await, Lsn(0x100));
        // Only the mapped safekeeper was asked.
        assert_eq!(*s.clients[&1].asked.lock().unwrap(), 1);
    }
}
