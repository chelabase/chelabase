//! The registry of runs in flight (per user and per target limits, cancellation).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chelabase_types::BranchId;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{RUNS_PER_TARGET, RUNS_PER_USER};

type Runs = Mutex<HashMap<(String, Uuid), Entry>>;

struct Entry {
    branch: BranchId,
    /// The member who started the run (`None`: no member row, platform mode).
    member: Option<Uuid>,
    database: String,
    cancel: CancellationToken,
}

/// The runs in flight, keyed by (platform user, run id).
#[derive(Default)]
pub struct RunRegistry {
    runs: Arc<Runs>,
}

/// Who asks to cancel a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Canceller {
    /// A member: only the runs they started.
    Member(Uuid),
    /// An Admin: any run.
    Admin,
}

/// What a cancel request found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cancelled {
    /// The token was fired.
    Done,
    /// A run with this id is in flight on the branch, but someone else started it.
    NotYours,
    /// No such run in flight on the branch.
    Missing,
}

/// Why a run can't start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReserveError {
    /// This user already has a run with this id in flight.
    InUse,
    /// This user has `RUNS_PER_USER` runs in flight.
    UserLimit,
    /// The (branch, database) has `RUNS_PER_TARGET` runs in flight.
    TargetLimit,
}

/// A reserved slot; dropping it removes the run from the registry.
pub struct RunGuard {
    runs: Arc<Runs>,
    key: (String, Uuid),
    cancel: CancellationToken,
}

impl RunRegistry {
    /// Reserves a slot for `run_id`, checking the duplicate and both limits under one lock.
    pub fn reserve(
        &self,
        user: &str,
        run_id: Uuid,
        member: Option<Uuid>,
        branch: BranchId,
        database: &str,
    ) -> Result<RunGuard, ReserveError> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let key = (user.to_string(), run_id);
        if runs.contains_key(&key) {
            return Err(ReserveError::InUse);
        }
        if runs.keys().filter(|(u, _)| u == user).count() >= RUNS_PER_USER {
            return Err(ReserveError::UserLimit);
        }
        let on_target = runs
            .values()
            .filter(|e| e.branch == branch && e.database == database)
            .count();
        if on_target >= RUNS_PER_TARGET {
            return Err(ReserveError::TargetLimit);
        }
        let cancel = CancellationToken::new();
        runs.insert(
            key.clone(),
            Entry {
                branch,
                member,
                database: database.to_string(),
                cancel: cancel.clone(),
            },
        );
        Ok(RunGuard {
            runs: self.runs.clone(),
            key,
            cancel,
        })
    }

    /// Fires the token of run `run_id` on `branch` when `by` may cancel it: its starter, or an
    /// Admin.
    pub fn cancel(&self, run_id: Uuid, branch: BranchId, by: Canceller) -> Cancelled {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let on_branch: Vec<&Entry> = runs
            .iter()
            .filter(|((_, id), e)| *id == run_id && e.branch == branch)
            .map(|(_, e)| e)
            .collect();
        let mut found = Cancelled::Missing;
        for entry in on_branch {
            let allowed = match by {
                Canceller::Admin => true,
                Canceller::Member(member) => entry.member == Some(member),
            };
            if allowed {
                entry.cancel.cancel();
                found = Cancelled::Done;
            } else if found == Cancelled::Missing {
                found = Cancelled::NotYours;
            }
        }
        found
    }
}

impl RunGuard {
    /// The run's cancellation token (fired by [`RunRegistry::cancel`]).
    pub fn token(&self) -> CancellationToken {
        self.cancel.clone()
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        runs.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::BranchId;
    use uuid::Uuid;

    #[test]
    fn reserve_refuses_duplicates_and_caps() {
        let registry = RunRegistry::default();
        let branch = BranchId::new();
        let other = BranchId::new();
        let id = Uuid::new_v4();
        let first = registry.reserve("alice", id, None, branch, "main").unwrap();
        assert!(matches!(
            registry.reserve("alice", id, None, other, "main"),
            Err(ReserveError::InUse)
        ));
        let _a2 = registry
            .reserve("alice", Uuid::new_v4(), None, other, "main")
            .unwrap();
        let _a3 = registry
            .reserve("alice", Uuid::new_v4(), None, other, "main")
            .unwrap();
        let _a4 = registry
            .reserve("alice", Uuid::new_v4(), None, other, "main")
            .unwrap();
        assert!(matches!(
            registry.reserve("alice", Uuid::new_v4(), None, other, "app"),
            Err(ReserveError::UserLimit)
        ));
        // `other`/main now has 3 of alice's runs; bob adds the 4th, the 5th is refused.
        let _b1 = registry
            .reserve("bob", Uuid::new_v4(), None, other, "main")
            .unwrap();
        assert!(matches!(
            registry.reserve("bob", Uuid::new_v4(), None, other, "main"),
            Err(ReserveError::TargetLimit)
        ));
        // Another database on the same branch is another target.
        let _b2 = registry
            .reserve("bob", Uuid::new_v4(), None, other, "app")
            .unwrap();
        drop(first);
        let _again = registry.reserve("alice", id, None, branch, "main").unwrap();
    }

    #[test]
    fn cancel_fires_only_for_the_starter_or_an_admin() {
        let registry = RunRegistry::default();
        let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
        let (id, branch) = (Uuid::new_v4(), BranchId::new());
        let guard = registry
            .reserve("alice", id, Some(alice), branch, "main")
            .unwrap();
        assert_eq!(registry.cancel(id, branch, Canceller::Member(bob)), Cancelled::NotYours);
        assert!(!guard.token().is_cancelled());
        assert_eq!(
            registry.cancel(Uuid::new_v4(), branch, Canceller::Member(alice)),
            Cancelled::Missing
        );
        // Another branch's run isn't this branch's.
        assert_eq!(
            registry.cancel(id, BranchId::new(), Canceller::Admin),
            Cancelled::Missing
        );
        assert_eq!(registry.cancel(id, branch, Canceller::Member(alice)), Cancelled::Done);
        assert!(guard.token().is_cancelled());
        drop(guard);
        assert_eq!(registry.cancel(id, branch, Canceller::Admin), Cancelled::Missing);

        // An Admin cancels another member's run; a run with no member is only an Admin's.
        let guard = registry.reserve("carol", id, None, branch, "main").unwrap();
        assert_eq!(registry.cancel(id, branch, Canceller::Member(alice)), Cancelled::NotYours);
        assert_eq!(registry.cancel(id, branch, Canceller::Admin), Cancelled::Done);
        assert!(guard.token().is_cancelled());
    }
}
