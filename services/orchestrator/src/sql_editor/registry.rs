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
    database: String,
    cancel: CancellationToken,
}

/// The runs in flight, keyed by (platform user, run id).
#[derive(Default)]
pub struct RunRegistry {
    runs: Arc<Runs>,
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

    /// Fires the run's token; false when this user has no such run.
    pub fn cancel(&self, user: &str, run_id: Uuid) -> bool {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(&(user.to_string(), run_id)) {
            Some(entry) => {
                entry.cancel.cancel();
                true
            }
            None => false,
        }
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
        let first = registry.reserve("alice", id, branch, "main").unwrap();
        assert!(matches!(
            registry.reserve("alice", id, other, "main"),
            Err(ReserveError::InUse)
        ));
        let _a2 = registry
            .reserve("alice", Uuid::new_v4(), other, "main")
            .unwrap();
        let _a3 = registry
            .reserve("alice", Uuid::new_v4(), other, "main")
            .unwrap();
        let _a4 = registry
            .reserve("alice", Uuid::new_v4(), other, "main")
            .unwrap();
        assert!(matches!(
            registry.reserve("alice", Uuid::new_v4(), other, "app"),
            Err(ReserveError::UserLimit)
        ));
        // `other`/main now has 3 of alice's runs; bob adds the 4th, the 5th is refused.
        let _b1 = registry
            .reserve("bob", Uuid::new_v4(), other, "main")
            .unwrap();
        assert!(matches!(
            registry.reserve("bob", Uuid::new_v4(), other, "main"),
            Err(ReserveError::TargetLimit)
        ));
        // Another database on the same branch is another target.
        let _b2 = registry
            .reserve("bob", Uuid::new_v4(), other, "app")
            .unwrap();
        drop(first);
        let _again = registry.reserve("alice", id, branch, "main").unwrap();
    }

    #[test]
    fn cancel_fires_only_the_callers_run() {
        let registry = RunRegistry::default();
        let id = Uuid::new_v4();
        let guard = registry
            .reserve("alice", id, BranchId::new(), "main")
            .unwrap();
        assert!(!registry.cancel("bob", id));
        assert!(!guard.token().is_cancelled());
        assert!(!registry.cancel("alice", Uuid::new_v4()));
        assert!(registry.cancel("alice", id));
        assert!(guard.token().is_cancelled());
        drop(guard);
        assert!(!registry.cancel("alice", id));
    }
}
