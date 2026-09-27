//! Reconcile decisions: compare metadata rows with what Docker actually has.

use cheladb_metadata::{DatabaseStatus, LiveDatabase};
use cheladb_types::DatabaseId;
use std::collections::HashSet;

pub const STALE_PROVISION_SECS: i64 = 600;
pub const STALE_DELETE_SECS: i64 = 60;

/// Managed resources found in Docker, by the database they belong to.
#[derive(Debug, Default)]
pub struct Observed {
    pub containers: HashSet<DatabaseId>,
    pub volumes: HashSet<DatabaseId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Pending/provisioning for too long: fail it and remove its resources.
    FailStale(DatabaseId),
    /// Ready, but its container is gone.
    FailMissingContainer(DatabaseId),
    /// Stuck in `deleting` (the delete job was lost): run the delete again.
    RetryDelete(DatabaseId),
    /// Container and/or volume of a database whose row is a `deleted` tombstone.
    RemoveOrphan(DatabaseId),
    /// Container and/or volume with no metadata row at all (e.g. metadata was
    /// reset or restored from an older backup). Reported, never removed: it
    /// may hold the only copy of someone's data.
    ReportUnknown(DatabaseId),
}

/// `deleted` holds the ids among `observed` whose rows are tombstones.
pub fn plan(rows: &[LiveDatabase], deleted: &HashSet<DatabaseId>, observed: &Observed) -> Vec<Action> {
    let mut actions = Vec::new();
    for row in rows {
        match row.status {
            DatabaseStatus::Pending | DatabaseStatus::Provisioning
                if row.age_secs > STALE_PROVISION_SECS =>
            {
                actions.push(Action::FailStale(row.id))
            }
            DatabaseStatus::Ready if !observed.containers.contains(&row.id) => {
                actions.push(Action::FailMissingContainer(row.id))
            }
            DatabaseStatus::Deleting if row.age_secs > STALE_DELETE_SECS => {
                actions.push(Action::RetryDelete(row.id))
            }
            _ => {}
        }
    }

    // Managed resources whose database has no live row: remove them only when
    // the row is a tombstone; one action per database even when both the
    // container and the volume remain.
    let live: HashSet<DatabaseId> = rows.iter().map(|r| r.id).collect();
    let mut orphans: Vec<DatabaseId> = observed
        .containers
        .union(&observed.volumes)
        .filter(|id| !live.contains(id))
        .copied()
        .collect();
    orphans.sort_by_key(|id| id.to_string());
    actions.extend(orphans.into_iter().map(|id| {
        if deleted.contains(&id) {
            Action::RemoveOrphan(id)
        } else {
            Action::ReportUnknown(id)
        }
    }));
    actions
}

/// A running provision must stop (and clean up) once a delete has begun.
pub fn should_abort_provision(status: DatabaseStatus) -> bool {
    matches!(status, DatabaseStatus::Deleting | DatabaseStatus::Deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live(id: DatabaseId, status: DatabaseStatus, age_secs: i64) -> LiveDatabase {
        LiveDatabase { id, status, age_secs }
    }

    fn sorted(mut actions: Vec<Action>) -> Vec<Action> {
        actions.sort_by_key(|a| format!("{a:?}"));
        actions
    }

    #[test]
    fn fresh_provisioning_is_left_alone_even_with_its_container() {
        let id = DatabaseId::new();
        let observed = Observed { containers: [id].into(), volumes: [id].into() };
        assert!(plan(&[live(id, DatabaseStatus::Pending, 5)], &HashSet::new(), &observed).is_empty());
    }

    #[test]
    fn stale_provisioning_fails() {
        let id = DatabaseId::new();
        assert_eq!(
            plan(&[live(id, DatabaseStatus::Provisioning, 601)], &HashSet::new(), &Observed::default()),
            vec![Action::FailStale(id)]
        );
    }

    #[test]
    fn ready_without_container_fails() {
        let id = DatabaseId::new();
        let observed = Observed { containers: HashSet::new(), volumes: [id].into() };
        assert_eq!(
            plan(&[live(id, DatabaseStatus::Ready, 3600)], &HashSet::new(), &observed),
            vec![Action::FailMissingContainer(id)]
        );
    }

    #[test]
    fn ready_with_container_is_fine() {
        let id = DatabaseId::new();
        let observed = Observed { containers: [id].into(), volumes: [id].into() };
        assert!(plan(&[live(id, DatabaseStatus::Ready, 3600)], &HashSet::new(), &observed).is_empty());
    }

    #[test]
    fn lost_delete_is_retried_after_a_minute() {
        let id = DatabaseId::new();
        assert!(plan(&[live(id, DatabaseStatus::Deleting, 30)], &HashSet::new(), &Observed::default()).is_empty());
        assert_eq!(
            plan(&[live(id, DatabaseStatus::Deleting, 61)], &HashSet::new(), &Observed::default()),
            vec![Action::RetryDelete(id)]
        );
    }

    // Review Focus 4.
    #[test]
    fn only_resources_of_deleted_databases_are_removed() {
        let (live_id, deleted_c, deleted_v) = (DatabaseId::new(), DatabaseId::new(), DatabaseId::new());
        let observed = Observed {
            containers: [live_id, deleted_c].into(),
            volumes: [live_id, deleted_v].into(),
        };
        let deleted = [deleted_c, deleted_v].into();
        assert_eq!(
            sorted(plan(&[live(live_id, DatabaseStatus::Pending, 1)], &deleted, &observed)),
            sorted(vec![Action::RemoveOrphan(deleted_c), Action::RemoveOrphan(deleted_v)])
        );
    }

    #[test]
    fn a_deleted_database_with_both_container_and_volume_is_removed_once() {
        let id = DatabaseId::new();
        let observed = Observed { containers: [id].into(), volumes: [id].into() };
        assert_eq!(plan(&[], &[id].into(), &observed), vec![Action::RemoveOrphan(id)]);
    }

    // A metadata reset or restored backup must never wipe tenant data.
    #[test]
    fn resources_without_any_row_are_reported_not_removed() {
        let id = DatabaseId::new();
        let observed = Observed { containers: [id].into(), volumes: [id].into() };
        assert_eq!(plan(&[], &HashSet::new(), &observed), vec![Action::ReportUnknown(id)]);
    }

    // Review Focus 1.
    #[test]
    fn provisioning_aborts_once_delete_has_started() {
        assert!(should_abort_provision(DatabaseStatus::Deleting));
        assert!(should_abort_provision(DatabaseStatus::Deleted));
        assert!(!should_abort_provision(DatabaseStatus::Provisioning));
        assert!(!should_abort_provision(DatabaseStatus::Pending));
    }
}
