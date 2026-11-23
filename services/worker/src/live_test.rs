//! #337: live-test databases, which the stack worker's periodic passes leave to their tests.
//!
//! Live tests (`rest_live`, `fn_triggers_live`) provision databases in the workbench stack's
//! shared metadata database and drive their own bootstraps, resyncs and trigger syncs from an
//! in-process worker. The stack's own worker would otherwise health-check, bootstrap, resync
//! and settle those branches between a test's steps. With
//! `CHELABASE_WORKER_SKIP_LIVE_TEST_DATABASES` on (set by the workbench overlay alone, off by
//! default and never in the self-hosted base file), its periodic passes skip them.
//!
//! The marker is the database id, which only a test helper can choose: every database the
//! platform makes gets [`DatabaseId::new`] (a random version 4 UUID; the orchestrator's
//! create handler, and nothing takes an id from a caller), while [`database_id`] makes a
//! version 8 UUID whose last six bytes are [`TAG`]. A version 4 id can never match, so no
//! tenant can mark a database, whatever it names it. With the setting off nothing here is
//! consulted: [`LiveTests::new`] then skips nothing.

use std::collections::HashSet;

use chelabase_types::{BranchId, DatabaseId};
use uuid::Uuid;

use crate::reconcile::Action;

/// The worker setting (a boolean, default off).
pub const ENV: &str = "CHELABASE_WORKER_SKIP_LIVE_TEST_DATABASES";

/// The last six bytes of every live-test database id ("7e57 db" twice: test db).
pub const TAG: [u8; 6] = [0x7e, 0x57, 0xdb, 0x7e, 0x57, 0xdb];

/// A new live-test database id: random, except for the UUID version (8), its variant and
/// [`TAG`]. The leading hex stays random, so names the tests derive from the id's prefix
/// stay unique.
pub fn database_id() -> DatabaseId {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = 0x80 | (bytes[6] & 0x0f);
    bytes[8] = 0x80 | (bytes[8] & 0x3f);
    bytes[10..].copy_from_slice(&TAG);
    DatabaseId(Uuid::from_bytes(bytes))
}

/// Whether `id` is a live-test id ([`database_id`]): version 8, RFC variant, [`TAG`] last.
pub fn is_live_test_id(id: Uuid) -> bool {
    id.get_version_num() == 8
        && id.get_variant() == uuid::Variant::RFC4122
        && id.as_bytes()[10..] == TAG
}

/// The branches one pass leaves alone: every branch of a live-test database, and only while
/// the setting is on.
#[derive(Debug, Default, Clone)]
pub struct LiveTests {
    on: bool,
    branches: HashSet<BranchId>,
}

impl LiveTests {
    /// Skips nothing.
    pub fn off() -> Self {
        Self::default()
    }

    /// From `(branch, its database)` pairs. `on` false (the setting's default) skips nothing,
    /// whatever the ids.
    pub fn new(on: bool, branches: impl IntoIterator<Item = (BranchId, DatabaseId)>) -> Self {
        if !on {
            return Self::off();
        }
        let branches = branches
            .into_iter()
            .filter(|(_, database)| is_live_test_id(database.as_uuid()))
            .map(|(branch, _)| branch)
            .collect();
        Self { on, branches }
    }

    /// Whether the pass leaves `branch` to its test: one listed with a live-test database, or
    /// a main branch (its id is the database's) not listed yet.
    pub fn skips(&self, branch: BranchId) -> bool {
        self.on && (self.branches.contains(&branch) || is_live_test_id(branch.as_uuid()))
    }

    /// Whether a reconcile pass drops `action` ([`crate::reconcile::Action`]): the health
    /// checks' bootstraps, enable retries and resyncs of a skipped branch. Every other action
    /// stands, for test databases too: the cleanups (a stale provision failed, a lost delete or
    /// teardown run again) still reap what a crashed test leaves, and the compute and storage
    /// repairs keep its compute serving. No wildcard: a new action is decided here.
    pub fn skips_action(&self, action: &Action) -> bool {
        match action {
            Action::BootstrapDataApi { branch_id, .. } => self.skips(*branch_id),
            Action::RetryEnableDataApi(branch, _) => self.skips(*branch),
            Action::Resync(branch, _) => self.skips(*branch),
            Action::FailStale(_)
            | Action::FailStaleBranch(_)
            | Action::RetryDelete(_)
            | Action::RetryBranchDelete(_)
            | Action::FinishSwitch(_)
            | Action::RecreateBranchCompute(..)
            | Action::UpgradeCompute(..)
            | Action::ApplyPending(_)
            | Action::RemoveCompute(_)
            | Action::RetryTeardown(_)
            | Action::RemoveStaticCompute(_)
            | Action::RemoveRehearsal(_)
            | Action::PatchHistoryWindow(_)
            | Action::ReportStuckRestore(_)
            | Action::ReportUnknownTenant(_)
            | Action::ReportUnknownTimeline(..)
            | Action::ReportEndpointWithoutDatabase(_)
            | Action::ReportMissingTenant { .. } => false,
        }
    }

    /// The listed live-test branches, sorted (for the log).
    pub fn branches(&self) -> Vec<BranchId> {
        let mut ids: Vec<BranchId> = self.branches.iter().copied().collect();
        ids.sort_by_key(|id| id.as_uuid());
        ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_ids_are_never_live_test_ids() {
        for _ in 0..100_000 {
            let id = DatabaseId::new();
            assert_eq!(id.as_uuid().get_version_num(), 4);
            assert!(!is_live_test_id(id.as_uuid()), "{id}");
            assert!(!is_live_test_id(BranchId::new().as_uuid()));
        }
        // A version 4 id ending in the tag still isn't one.
        let mut bytes = *DatabaseId::new().as_uuid().as_bytes();
        bytes[10..].copy_from_slice(&TAG);
        assert!(!is_live_test_id(Uuid::from_bytes(bytes)));
        // Nor a version 8 id without it.
        let mut bytes = *database_id().as_uuid().as_bytes();
        bytes[15] ^= 1;
        assert!(!is_live_test_id(Uuid::from_bytes(bytes)));
    }

    #[test]
    fn live_test_ids_are_marked_and_keep_a_random_prefix() {
        let ids: Vec<DatabaseId> = (0..64).map(|_| database_id()).collect();
        for id in &ids {
            assert!(is_live_test_id(id.as_uuid()), "{id}");
            assert!(id.to_string().ends_with("-7e57db7e57db"), "{id}");
        }
        let prefixes: HashSet<String> = ids
            .iter()
            .map(|id| id.to_string()[..6].to_string())
            .collect();
        assert!(
            prefixes.len() > 60,
            "the leading hex is random: {prefixes:?}"
        );
    }

    #[test]
    fn marked_branches_are_skipped_only_with_the_setting_on() {
        let test_db = database_id();
        let test_child = BranchId::new();
        let real_db = DatabaseId::new();
        let real_child = BranchId::new();
        let listed = [
            (BranchId::from(test_db), test_db),
            (test_child, test_db),
            (BranchId::from(real_db), real_db),
            (real_child, real_db),
        ];
        // Off (self-hosted, the default): nothing is skipped, marked or not.
        let off = LiveTests::new(false, listed);
        for (branch, _) in listed {
            assert!(!off.skips(branch), "{branch}");
        }
        assert!(
            !off.skips(BranchId::from(database_id())),
            "an unlisted marked main"
        );
        assert!(!LiveTests::off().skips(test_child));
        // On: exactly the live-test database's branches, children included.
        let on = LiveTests::new(true, listed);
        assert!(on.skips(BranchId::from(test_db)));
        assert!(on.skips(test_child));
        assert!(!on.skips(BranchId::from(real_db)));
        assert!(!on.skips(real_child));
        assert!(
            on.skips(BranchId::from(database_id())),
            "an unlisted marked main"
        );
        assert!(!on.skips(BranchId::new()), "an unlisted platform branch");
        let mut expected = vec![BranchId::from(test_db), test_child];
        expected.sort_by_key(|id| id.as_uuid());
        assert_eq!(on.branches(), expected);
    }
}
