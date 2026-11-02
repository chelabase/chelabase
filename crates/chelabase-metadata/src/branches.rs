//! Branches: each is a Neon timeline with its own endpoint, compute and owner password. A
//! database's main branch has the database's id and no parent.
//!
//! **Lock order.** A transaction that touches both tables locks the `databases` row before
//! any `branches` row, and `branches` rows before `neon_*` rows. Take the database row first
//! (`FOR NO KEY UPDATE`, or by updating it) even when the branch is what the transaction is
//! about: the reverse order deadlocks against `begin_delete`, `set_status` and
//! `create_branch`. See [`lock_database_of_main`].
//!
//! Use `FOR NO KEY UPDATE`, never `FOR UPDATE`, on `databases`: inserting a branch takes a
//! KEY SHARE lock on its database row (the foreign key), which `FOR UPDATE` would block.

use std::collections::HashSet;
use std::time::Duration;

use chelabase_types::{BranchId, DatabaseId, TenantId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{DatabaseStatus, MetadataClient, MetadataError, NewEndpoint, RoleOrigin};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum BranchStatus {
    Pending,
    Provisioning,
    Ready,
    Resetting,
    Failed,
    Deleting,
    Deleted,
}

impl BranchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Resetting => "resetting",
            Self::Failed => "failed",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "pending" => Ok(Self::Pending),
            "provisioning" => Ok(Self::Provisioning),
            "ready" => Ok(Self::Ready),
            "resetting" => Ok(Self::Resetting),
            "failed" => Ok(Self::Failed),
            "deleting" => Ok(Self::Deleting),
            "deleted" => Ok(Self::Deleted),
            other => Err(MetadataError::InvalidStatus(other.to_string())),
        }
    }
}

/// Main mirrors its database's lifecycle while provisioning and deleting.
impl From<DatabaseStatus> for BranchStatus {
    fn from(status: DatabaseStatus) -> Self {
        match status {
            DatabaseStatus::Pending => Self::Pending,
            DatabaseStatus::Provisioning => Self::Provisioning,
            DatabaseStatus::Ready => Self::Ready,
            DatabaseStatus::Failed => Self::Failed,
            DatabaseStatus::Deleting => Self::Deleting,
            DatabaseStatus::Deleted => Self::Deleted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum BranchKind {
    Normal,
    /// Made by a restore: the timeline the branch had before. No endpoint, no compute.
    Backup,
    /// A throwaway child of a merge's target, where its plan is rehearsed. Hidden from
    /// listings, not counted against `max_branches`, never routable.
    Rehearsal,
}

impl BranchKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Backup => "backup",
            Self::Rehearsal => "rehearsal",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "normal" => Ok(Self::Normal),
            "backup" => Ok(Self::Backup),
            "rehearsal" => Ok(Self::Rehearsal),
            other => Err(MetadataError::InvalidBranchKind(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BranchRecord {
    pub id: BranchId,
    pub database_id: DatabaseId,
    pub name: String,
    /// `None` only for main.
    pub parent_branch_id: Option<BranchId>,
    pub kind: BranchKind,
    pub status: BranchStatus,
    /// 32 hex; `None` until created.
    pub timeline_id: Option<String>,
    /// Created by a reset or restore, not yet switched to.
    pub pending_timeline_id: Option<String>,
    /// When `pending_timeline_id` was recorded; `None` without one.
    pub pending_timeline_at: Option<DateTime<Utc>>,
    /// Switched away from by a reset, not yet deleted.
    pub superseded_timeline_id: Option<String>,
    /// `None` for backups, and until provisioning records one.
    pub endpoint_id: Option<String>,
    /// Owner password, sealed with `chelabase-secrets` (aad = branch id).
    pub owner_password_enc: Option<String>,
    /// What the user asked for, if a time.
    pub branched_at_time: Option<DateTime<Utc>>,
    /// What it resolved to.
    pub branched_at_lsn: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Where the branch's history begins: the commit time at its timeline's branch point, or
    /// when the timeline was made. `None` when unknown.
    pub history_start: Option<DateTime<Utc>>,
    /// The `authenticator` role's password on this branch, sealed with `chelabase-secrets`
    /// (purpose `authenticator`); `None` until the data API is set up for it.
    pub authenticator_password_enc: Option<String>,
    /// Refused by delete and reset-to-parent (restore still works).
    pub protected: bool,
    /// Login roles other than the owner, the maintenance roles and the exempt ones can't run
    /// schema changes here (the schema guard's login-role rule).
    pub guard_logins: bool,
    /// The hash of the protection review last acknowledged, if any.
    pub review_ack_hash: Option<String>,
    /// The item keys of that acknowledged review.
    pub review_ack_keys: Vec<String>,
}

/// A branch to create (always `normal`, status `pending`).
#[derive(Debug, Clone, Copy)]
pub struct NewBranch<'a> {
    pub id: BranchId,
    pub database_id: DatabaseId,
    pub name: &'a str,
    pub parent_branch_id: BranchId,
    pub branched_at_time: Option<DateTime<Utc>>,
    pub branched_at_lsn: Option<&'a str>,
}

/// Outcome of [`MetadataClient::create_branch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateBranchOutcome {
    Created,
    /// A live branch of this database already has that name.
    NameTaken,
    /// The database already has `max` live `normal` branches besides main.
    CapReached {
        max: i32,
    },
    /// The parent isn't a ready branch of this database.
    ParentNotReady,
    /// The database is deleting, deleted or missing.
    DatabaseNotLive,
}

/// Outcome of [`MetadataClient::claim_branch_for_switch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// ready → resetting.
    Claimed,
    /// The branch isn't ready (or is missing).
    NotReady,
    /// The branch is ready but protected, and the caller doesn't allow that.
    Protected,
    /// A merge holds the branch, or a rehearsal of it is still there (T14 F3, checked under
    /// the branch's row lock); nothing written.
    MergeBusy(crate::merges::BranchBusy),
}

/// Outcome of [`MetadataClient::begin_branch_delete`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchDeleteOutcome {
    /// This call moved the branch (and its live backups) to `deleting`.
    Started,
    /// Nothing moved: the branch was already deleting or deleted, or is missing. While it is
    /// deleting, a backup made since is still taken along.
    NotMoved,
    /// Refused, nothing written: the branch itself (`backup` is `None`) or its live backup
    /// `backup` has live `normal` children, named in `children`. Neon can't delete a timeline
    /// that others branch from.
    HasChildren {
        backup: Option<String>,
        children: Vec<String>,
    },
    /// Refused, nothing written: the branch is protected (a flag set after the caller's own
    /// check).
    Protected,
    /// Refused, nothing written: a merge holds the branch, or a rehearsal of it is still there
    /// (T14 F3, checked under the branch's row lock).
    MergeBusy(crate::merges::BranchBusy),
}

/// Why a merge keeps `branch_name` from being reset, restored or deleted, for a refusal the
/// worker records (the API answers `branch_merge_busy` with the ids instead).
pub fn merge_busy_refusal(branch_name: &str, busy: crate::merges::BranchBusy) -> String {
    match busy {
        crate::merges::BranchBusy::Merge(id) => format!(
            "merge {id} holds {branch_name}: it is rehearsing or applying, or failed half applied \
             (retry or abandon it)"
        ),
        crate::merges::BranchBusy::Rehearsal(id) => format!(
            "a merge's rehearsal of {branch_name} (branch {id}) is still being removed; try again \
             in a few minutes"
        ),
    }
}

impl BranchDeleteOutcome {
    /// The refusal message for deleting `branch_name`, when this is a refusal.
    pub fn refusal(&self, branch_name: &str) -> Option<String> {
        if *self == Self::Protected {
            return Some(format!(
                "cannot delete {branch_name}: it is protected; unprotect it first"
            ));
        }
        if let Self::MergeBusy(busy) = self {
            return Some(format!(
                "cannot delete {branch_name}: {}",
                merge_busy_refusal(branch_name, *busy)
            ));
        }
        let Self::HasChildren { backup, children } = self else {
            return None;
        };
        let children = children.join(", ");
        Some(match backup {
            None => format!(
                "cannot delete {branch_name}: it has child branches ({children}); delete them first"
            ),
            Some(backup) => format!(
                "cannot delete {branch_name}: its backup {backup} has child branches \
                 ({children}); delete them first"
            ),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseSettings {
    pub history_window: Duration,
    pub max_branches: i32,
}

/// A change to a database's settings; `None` keeps a field. The tables' CHECK constraints
/// bound the values (callers validate first for a friendly message).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsPatch {
    pub history_window: Option<Duration>,
    pub max_branches: Option<i32>,
    pub max_rows: Option<i32>,
    pub statement_timeout_ms: Option<i32>,
    pub max_connections: Option<i32>,
    pub storage_max_connections: Option<i32>,
}

/// A branch that isn't deleted, as seen by the reconcile loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveBranch {
    pub id: BranchId,
    pub database_id: DatabaseId,
    pub kind: BranchKind,
    pub status: BranchStatus,
    pub endpoint_id: Option<String>,
    pub timeline_id: Option<String>,
    pub pending_timeline_id: Option<String>,
    pub superseded_timeline_id: Option<String>,
    /// Seconds since the row last changed.
    pub age_secs: i64,
}

/// Every column of a `branches` row, as [`branch_from_row`] reads it.
const BRANCH_COLUMNS: &str = "id, database_id, name, parent_branch_id, kind, status, timeline_id, \
     pending_timeline_id, pending_timeline_at, superseded_timeline_id, endpoint_id, owner_password_enc, \
     branched_at_time, branched_at_lsn, error, created_at, updated_at, history_start, \
     authenticator_password_enc, protected, guard_logins, review_ack_hash, review_ack_keys";

fn branch_from_row(r: &PgRow) -> Result<BranchRecord, MetadataError> {
    Ok(BranchRecord {
        id: BranchId(r.get("id")),
        database_id: DatabaseId(r.get("database_id")),
        name: r.get("name"),
        parent_branch_id: r.get::<Option<Uuid>, _>("parent_branch_id").map(BranchId),
        kind: BranchKind::parse(r.get("kind"))?,
        status: BranchStatus::parse(r.get("status"))?,
        timeline_id: r.get("timeline_id"),
        pending_timeline_id: r.get("pending_timeline_id"),
        pending_timeline_at: r.get("pending_timeline_at"),
        superseded_timeline_id: r.get("superseded_timeline_id"),
        endpoint_id: r.get("endpoint_id"),
        owner_password_enc: r.get("owner_password_enc"),
        branched_at_time: r.get("branched_at_time"),
        branched_at_lsn: r.get("branched_at_lsn"),
        error: r.get("error"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
        history_start: r.get("history_start"),
        authenticator_password_enc: r.get("authenticator_password_enc"),
        protected: r.get("protected"),
        guard_logins: r.get("guard_logins"),
        review_ack_hash: r.get("review_ack_hash"),
        review_ack_keys: r.get("review_ack_keys"),
    })
}

/// Copies `from`'s guard exemptions onto `to` (a new branch row in the same transaction).
async fn copy_guard_exemptions(
    tx: &mut sqlx::PgConnection,
    from: Uuid,
    to: Uuid,
) -> Result<(), MetadataError> {
    sqlx::query(
        "INSERT INTO branch_guard_exemptions (branch_id, role_name)
         SELECT $2, role_name FROM branch_guard_exemptions WHERE branch_id = $1",
    )
    .bind(from)
    .bind(to)
    .execute(tx)
    .await?;
    Ok(())
}

/// Branches matching `condition` (on `$1`), oldest first.
async fn select_branches(
    client: &MetadataClient,
    condition: &'static str,
    id: Uuid,
) -> Result<Vec<BranchRecord>, MetadataError> {
    let sql =
        format!("SELECT {BRANCH_COLUMNS} FROM branches WHERE {condition} ORDER BY created_at, id");
    sqlx::query(AssertSqlSafe(sql))
        .bind(id)
        .fetch_all(&client.pool)
        .await?
        .iter()
        .map(branch_from_row)
        .collect()
}

impl MetadataClient {
    pub async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        let sql = format!("SELECT {BRANCH_COLUMNS} FROM branches WHERE id = $1");
        sqlx::query(AssertSqlSafe(sql))
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(branch_from_row)
            .transpose()
    }

    /// The database's branches that aren't deleted, oldest first.
    pub async fn list_branches(&self, db: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        select_branches(
            self,
            "database_id = $1 AND status <> 'deleted'",
            db.as_uuid(),
        )
        .await
    }

    /// One transaction: lock the database row (FOR NO KEY UPDATE, which serializes creates
    /// as FOR UPDATE would); refuse unless it's live (not deleting/deleted); the parent must be
    /// a live branch of this database with status ready; count live normal non-main branches
    /// against max_branches; insert status 'pending'. A unique-name violation → NameTaken.
    pub async fn create_branch(
        &self,
        b: &NewBranch<'_>,
    ) -> Result<CreateBranchOutcome, MetadataError> {
        let mut tx = self.pool.begin().await?;
        // Every create for this database queues here, so the count below can't go stale.
        let max: Option<i32> = sqlx::query_scalar(
            "SELECT max_branches FROM databases
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
             FOR NO KEY UPDATE",
        )
        .bind(b.database_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(max) = max else {
            return Ok(CreateBranchOutcome::DatabaseNotLive);
        };
        // FOR SHARE: a concurrent delete of the parent waits for this insert, then sees the child.
        let parent_ready: Option<bool> = sqlx::query_scalar(
            "SELECT status = 'ready' FROM branches
             WHERE id = $1 AND database_id = $2
             FOR SHARE",
        )
        .bind(b.parent_branch_id.as_uuid())
        .bind(b.database_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if parent_ready != Some(true) {
            return Ok(CreateBranchOutcome::ParentNotReady);
        }
        let taken: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM branches
                            WHERE database_id = $1 AND name = $2 AND status <> 'deleted')",
        )
        .bind(b.database_id.as_uuid())
        .bind(b.name)
        .fetch_one(&mut *tx)
        .await?;
        if taken {
            return Ok(CreateBranchOutcome::NameTaken);
        }
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM branches
             WHERE database_id = $1 AND status <> 'deleted' AND kind = 'normal'
               AND parent_branch_id IS NOT NULL",
        )
        .bind(b.database_id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if live >= i64::from(max) {
            return Ok(CreateBranchOutcome::CapReached { max });
        }
        let inserted = sqlx::query(
            "INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status,
                                   branched_at_time, branched_at_lsn, guard_logins)
             VALUES ($1, $2, $3, $4, 'normal', 'pending', $5, $6,
                     (SELECT guard_logins FROM branches WHERE id = $4))",
        )
        .bind(b.id.as_uuid())
        .bind(b.database_id.as_uuid())
        .bind(b.name)
        .bind(b.parent_branch_id.as_uuid())
        .bind(b.branched_at_time)
        .bind(b.branched_at_lsn)
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(sqlx::Error::Database(err)) if err.constraint() == Some("branches_live_name") => {
                return Ok(CreateBranchOutcome::NameTaken);
            }
            Err(err) => return Err(err.into()),
        }
        // A point-in-time snapshot: the parent's FOR SHARE lock doesn't block exemption writes
        // (they take only a key-share lock through the FK), so a concurrent add/remove may or
        // may not reach the child.
        copy_guard_exemptions(&mut tx, b.parent_branch_id.as_uuid(), b.id.as_uuid()).await?;
        tx.commit().await?;
        Ok(CreateBranchOutcome::Created)
    }

    /// Inserts a rehearsal branch (kind `rehearsal`, status `pending`) under `parent`, which
    /// must be a ready branch of the database (else `Conflict`). It doesn't count against `max_branches` and
    /// skips the cap check. A live branch of that name already → `BranchNameTaken`.
    pub async fn create_rehearsal_branch(
        &self,
        database_id: DatabaseId,
        parent: BranchId,
        name: &str,
        created_by: &str,
    ) -> Result<BranchRecord, MetadataError> {
        let _ = created_by; // `branches` has no author column; the merge row records its creator.
        let mut tx = self.pool.begin().await?;
        // The same lock as create_branch: a concurrent delete of the parent waits for this insert.
        let parent_ready: Option<bool> = sqlx::query_scalar(
            "SELECT status = 'ready' FROM branches
             WHERE id = $1 AND database_id = $2
             FOR SHARE",
        )
        .bind(parent.as_uuid())
        .bind(database_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if parent_ready != Some(true) {
            return Err(MetadataError::Conflict);
        }
        let new_id = BranchId::new().as_uuid();
        let inserted = sqlx::query(
            "INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status, guard_logins)
             VALUES ($1, $2, $3, $4, 'rehearsal', 'pending',
                     (SELECT guard_logins FROM branches WHERE id = $4))
             RETURNING *",
        )
        .bind(new_id)
        .bind(database_id.as_uuid())
        .bind(name)
        .bind(parent.as_uuid())
        .fetch_one(&mut *tx)
        .await;
        let row = match inserted {
            Ok(row) => row,
            Err(sqlx::Error::Database(err)) if err.constraint() == Some("branches_live_name") => {
                return Err(MetadataError::BranchNameTaken(name.to_string()));
            }
            Err(err) => return Err(err.into()),
        };
        // A point-in-time snapshot, as in `create_branch`: a concurrent exemption add/remove on
        // the parent (key-share lock only) may or may not reach this branch.
        copy_guard_exemptions(&mut tx, parent.as_uuid(), new_id).await?;
        tx.commit().await?;
        branch_from_row(&row)
    }

    /// Main of every database of the tenant that isn't deleted, with the database's live
    /// `normal` branch count (main included; backups and deleted branches excluded). One
    /// query for the landing page. Databases without a main row (none should exist) are absent.
    pub async fn main_branches_with_counts(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(BranchRecord, i64)>, MetadataError> {
        sqlx::query(
            "SELECT b.*,
                    (SELECT count(*) FROM branches c
                     WHERE c.database_id = b.database_id AND c.kind = 'normal'
                       AND c.status <> 'deleted') AS branch_count
             FROM branches b
             JOIN databases d ON d.id = b.id
             JOIN projects p ON p.id = d.project_id
             WHERE p.tenant_id = $1 AND d.status <> 'deleted'
             ORDER BY d.created_at",
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| Ok((branch_from_row(r)?, r.get("branch_count"))))
        .collect()
    }

    /// Live `normal` children only (backups excluded).
    pub async fn live_children(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        select_branches(
            self,
            "parent_branch_id = $1 AND kind = 'normal' AND status <> 'deleted'",
            id.as_uuid(),
        )
        .await
    }

    pub async fn live_backups_of(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        select_branches(
            self,
            "parent_branch_id = $1 AND kind = 'backup' AND status <> 'deleted'",
            id.as_uuid(),
        )
        .await
    }

    /// Guarded status change: `from` must match; never out of deleting/deleted. True when changed.
    pub async fn set_branch_status(
        &self,
        id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        let from: Vec<&str> = from.iter().map(|s| s.as_str()).collect();
        let updated = sqlx::query(
            "UPDATE branches SET status = $3, updated_at = now()
             WHERE id = $1 AND status = ANY($2) AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(id.as_uuid())
        .bind(&from)
        .bind(to.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// ready → resetting, clearing the last error: the API's claim before a reset or restore,
    /// so each attempt starts clean. With `allow_protected = false` (delete, reset-to-parent) a
    /// protected branch isn't claimed: the claim's `UPDATE` carries `AND NOT protected`, and a
    /// zero-row result is re-read to tell `Protected` (a ready, protected branch) from
    /// `NotReady`. Restore passes `true`.
    pub async fn claim_branch_for_switch(
        &self,
        id: BranchId,
        allow_protected: bool,
    ) -> Result<ClaimOutcome, MetadataError> {
        self.claim_branch_for_switch_except(id, allow_protected, None)
            .await
    }

    /// [`Self::claim_branch_for_switch`] where merge `except` doesn't hold the branch: the
    /// worker's after-merge reset of a merge's source, which that merge (`finishing`) holds
    /// itself (T14 F3, I1).
    pub async fn claim_branch_for_switch_except(
        &self,
        id: BranchId,
        allow_protected: bool,
        except: Option<Uuid>,
    ) -> Result<ClaimOutcome, MetadataError> {
        // The branch's row lock first, then the merges (T14 M12): a merge moving to a status
        // that holds the branch takes a share lock on this row, so either it committed before
        // the check below (seen) or it waits and then finds the branch no longer ready.
        let mut tx = self.pool.begin().await?;
        let locked: Option<String> =
            sqlx::query_scalar("SELECT status FROM branches WHERE id = $1 FOR UPDATE")
                .bind(id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        if locked.as_deref() == Some("ready")
            && let Some(busy) = crate::merges::branch_busy(&mut *tx, id, except).await?
        {
            return Ok(ClaimOutcome::MergeBusy(busy));
        }
        let updated = sqlx::query(
            "UPDATE branches SET status = 'resetting', error = NULL, updated_at = now()
             WHERE id = $1 AND status = 'ready' AND ($2 OR NOT protected)",
        )
        .bind(id.as_uuid())
        .bind(allow_protected)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
        if updated == 1 {
            return Ok(ClaimOutcome::Claimed);
        }
        let protected_and_ready: bool = sqlx::query_scalar(
            "SELECT coalesce((SELECT protected AND status = 'ready' FROM branches WHERE id = $1), false)",
        )
        .bind(id.as_uuid())
        .fetch_one(&self.pool)
        .await?;
        Ok(if protected_and_ready && !allow_protected {
            ClaimOutcome::Protected
        } else {
            ClaimOutcome::NotReady
        })
    }

    /// Sets the protected flag of a `normal` branch (main included) in any status but
    /// `deleting`/`deleted`. `None` when there's no such branch (or it's a backup, or being
    /// deleted); `Some(record)` after the update.
    pub async fn set_branch_protected(
        &self,
        id: BranchId,
        protected: bool,
    ) -> Result<Option<BranchRecord>, MetadataError> {
        let sql = format!(
            "UPDATE branches SET protected = $2, updated_at = now()
             WHERE id = $1 AND kind = 'normal' AND status NOT IN ('deleting', 'deleted')
             RETURNING {BRANCH_COLUMNS}"
        );
        sqlx::query(AssertSqlSafe(sql))
            .bind(id.as_uuid())
            .bind(protected)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(branch_from_row)
            .transpose()
    }

    /// Sets the login-role guard switch (same row guards as [`Self::set_branch_protected`]).
    pub async fn set_branch_guard_logins(
        &self,
        id: BranchId,
        on: bool,
    ) -> Result<Option<BranchRecord>, MetadataError> {
        let sql = format!(
            "UPDATE branches SET guard_logins = $2, updated_at = now()
             WHERE id = $1 AND kind = 'normal' AND status NOT IN ('deleting', 'deleted')
             RETURNING {BRANCH_COLUMNS}"
        );
        sqlx::query(AssertSqlSafe(sql))
            .bind(id.as_uuid())
            .bind(on)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(branch_from_row)
            .transpose()
    }

    /// The branch's guard-exempt role names, sorted (byte order).
    pub async fn list_guard_exemptions(&self, id: BranchId) -> Result<Vec<String>, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT role_name FROM branch_guard_exemptions
             WHERE branch_id = $1 ORDER BY role_name COLLATE \"C\"",
        )
        .bind(id.as_uuid())
        .fetch_all(&self.pool)
        .await?)
    }

    /// `true` when inserted, `false` when already exempt. An unknown branch fails with the FK error.
    pub async fn add_guard_exemption(&self, id: BranchId, role: &str) -> Result<bool, MetadataError> {
        let done = sqlx::query(
            "INSERT INTO branch_guard_exemptions (branch_id, role_name) VALUES ($1, $2)
             ON CONFLICT DO NOTHING",
        )
        .bind(id.as_uuid())
        .bind(role)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// `true` when removed, `false` when it wasn't exempt.
    pub async fn remove_guard_exemption(
        &self,
        id: BranchId,
        role: &str,
    ) -> Result<bool, MetadataError> {
        let done = sqlx::query(
            "DELETE FROM branch_guard_exemptions WHERE branch_id = $1 AND role_name = $2",
        )
        .bind(id.as_uuid())
        .bind(role)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// protected = true and the acknowledged list, in one UPDATE (same row guards as
    /// [`Self::set_branch_protected`]).
    pub async fn protect_branch(
        &self,
        id: BranchId,
        hash: &str,
        keys: &[String],
    ) -> Result<Option<BranchRecord>, MetadataError> {
        let sql = format!(
            "UPDATE branches SET protected = true, review_ack_hash = $2, review_ack_keys = $3,
                    updated_at = now()
             WHERE id = $1 AND kind = 'normal' AND status NOT IN ('deleting', 'deleted')
             RETURNING {BRANCH_COLUMNS}"
        );
        sqlx::query(AssertSqlSafe(sql))
            .bind(id.as_uuid())
            .bind(hash)
            .bind(keys)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(branch_from_row)
            .transpose()
    }

    /// Records the acknowledged review; nothing else changes.
    pub async fn acknowledge_review(
        &self,
        id: BranchId,
        hash: &str,
        keys: &[String],
    ) -> Result<Option<BranchRecord>, MetadataError> {
        let sql = format!(
            "UPDATE branches SET review_ack_hash = $2, review_ack_keys = $3, updated_at = now()
             WHERE id = $1 AND kind = 'normal' AND status NOT IN ('deleting', 'deleted')
             RETURNING {BRANCH_COLUMNS}"
        );
        sqlx::query(AssertSqlSafe(sql))
            .bind(id.as_uuid())
            .bind(hash)
            .bind(keys)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(branch_from_row)
            .transpose()
    }

    /// Not once a delete has begun.
    pub async fn set_branch_failed(&self, id: BranchId, error: &str) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE branches SET status = 'failed', error = $2, updated_at = now()
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(id.as_uuid())
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records (or clears, with None) the last error without changing the status; used when a
    /// reset or restore fails before its switch and the branch goes back to ready.
    pub async fn set_branch_error(
        &self,
        id: BranchId,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE branches SET error = $2, updated_at = now()
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(id.as_uuid())
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// → deleting unless already deleting or deleted; also the branch's live backups. Refused
    /// (nothing written) while the branch or one of its live backups has live `normal`
    /// children, or while a merge holds the live branch (T14 F3: checked under its row lock).
    pub async fn begin_branch_delete(
        &self,
        id: BranchId,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        self.begin_branch_delete_except(id, None).await
    }

    /// [`Self::begin_branch_delete`] where merge `except` doesn't hold the branch: the worker's
    /// after-merge delete of a merge's source, which that merge holds itself (T14 F3, I1).
    pub async fn begin_branch_delete_except(
        &self,
        id: BranchId,
        except: Option<Uuid>,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        let mut tx = self.pool.begin().await?;
        // The database row first (the lock order above): begin_delete takes it, then every
        // branch row in its scan's order, which may reach a backup before its branch.
        sqlx::query(
            "SELECT 1 FROM databases
             WHERE id = (SELECT database_id FROM branches WHERE id = $1)
             FOR NO KEY UPDATE",
        )
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
        // Children only arrive through create_branch, which takes the database row first: the
        // check below can't go stale before the commit. An already-deleting branch passed it.
        // The branch's own row lock (after the database's, the order above), then the merges:
        // a merge moving to a status that holds the branch takes a share lock on this row, so
        // either it committed before the busy check (seen) or it waits and then finds the
        // branch deleting (T14 M12, F3 I1).
        let live: bool = sqlx::query_scalar::<_, bool>(
            "SELECT status NOT IN ('deleting', 'deleted') FROM branches WHERE id = $1 FOR UPDATE",
        )
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(false);
        if live && let Some(busy) = crate::merges::branch_busy(&mut *tx, id, except).await? {
            return Ok(BranchDeleteOutcome::MergeBusy(busy));
        }
        if live {
            let blocked: Vec<(bool, String, String)> = sqlx::query_as(
                "SELECT p.id = $1, p.name, c.name
                 FROM branches c JOIN branches p ON p.id = c.parent_branch_id
                 WHERE c.kind = 'normal' AND c.status <> 'deleted'
                   AND (p.id = $1
                        OR (p.parent_branch_id = $1 AND p.kind = 'backup'
                            AND p.status <> 'deleted'))
                 ORDER BY p.id = $1 DESC, p.created_at, p.id, c.name",
            )
            .bind(id.as_uuid())
            .fetch_all(&mut *tx)
            .await?;
            if let Some((own, parent, _)) = blocked.first().cloned() {
                let children = blocked
                    .into_iter()
                    .filter(|(_, p, _)| *p == parent)
                    .map(|(_, _, child)| child)
                    .collect();
                return Ok(BranchDeleteOutcome::HasChildren {
                    backup: (!own).then_some(parent),
                    children,
                });
            }
        }
        // A new attempt: an error left by an earlier one no longer applies. Never a protected
        // branch (a flag set after the caller's check): a zero-row result is re-read.
        let moved = sqlx::query(
            "UPDATE branches SET status = 'deleting', error = NULL, updated_at = now()
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted') AND NOT protected",
        )
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved == 0 {
            let protected: bool = sqlx::query_scalar(
                "SELECT coalesce((SELECT protected AND status NOT IN ('deleting', 'deleted')
                                  FROM branches WHERE id = $1), false)",
            )
            .bind(id.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
            if protected {
                return Ok(BranchDeleteOutcome::Protected);
            }
        }
        // Also when an earlier call moved the branch: a backup made since goes too.
        sqlx::query(
            "UPDATE branches SET status = 'deleting', updated_at = now()
             WHERE parent_branch_id = $1 AND kind = 'backup'
               AND status NOT IN ('deleting', 'deleted')
               AND EXISTS (SELECT 1 FROM branches WHERE id = $1 AND status = 'deleting')",
        )
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(if moved == 1 {
            BranchDeleteOutcome::Started
        } else {
            BranchDeleteOutcome::NotMoved
        })
    }

    /// Clears owner_password_enc, sets deleted_at.
    pub async fn mark_branch_deleted(&self, id: BranchId) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE branches
             SET status = 'deleted', deleted_at = now(), updated_at = now(),
                 owner_password_enc = NULL
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One transaction: endpoint + role + database rows, link to the branch, and record
    /// timeline_id. False (nothing written) if the branch is deleting/deleted or already linked.
    /// For main, the database row also records the database name and owner role.
    ///
    /// `parent_protected_seen` is the parent's `protected` flag as the caller read it when it
    /// chose the role copies; the transaction reads it again (`FOR SHARE`) and returns
    /// [`MetadataError::ParentProtectionChanged`] (nothing written) when it differs. A branch
    /// with no parent (main) skips the check.
    ///
    /// Lock order: the database row first (main's own row, or the child's database row), then
    /// the branch rows, the same order `begin_delete` uses, so the two cannot deadlock.
    pub async fn create_endpoint_for_branch(
        &self,
        id: BranchId,
        timeline_id: &str,
        ep: &NewEndpoint<'_>,
        parent_protected_seen: bool,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_database_of_branch(&mut tx, id).await?;
        let claimed: Option<Uuid> = sqlx::query_scalar(
            "SELECT database_id FROM branches
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted') AND endpoint_id IS NULL
             FOR UPDATE",
        )
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(database_id) = claimed else {
            return Ok(false);
        };
        let parent_protected: Option<bool> = sqlx::query_scalar(
            "SELECT p.protected FROM branches c JOIN branches p ON p.id = c.parent_branch_id
             WHERE c.id = $1 FOR SHARE OF p",
        )
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if parent_protected.is_some_and(|p| p != parent_protected_seen) {
            return Err(MetadataError::ParentProtectionChanged);
        }
        sqlx::query(
            "INSERT INTO neon_endpoints
                 (id, tenant_id, timeline_id, compute_token_sha256, routable, static_lsn)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(ep.id)
        .bind(ep.tenant_id)
        .bind(ep.timeline_id)
        .bind(ep.compute_token_sha256)
        .bind(ep.routable)
        .bind(ep.static_lsn)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin, level)
             VALUES ($1, $2, $3, 'owner', 'owner_level')",
        )
        .bind(ep.id)
        .bind(ep.owner_role)
        .bind(ep.owner_verifier)
        .execute(&mut *tx)
        .await?;
        // S1: a personal row is never copied; a child's comes from its reconcile, for a login
        // whose member qualifies there.
        for copy in ep.role_copies.iter().filter(|c| c.origin != RoleOrigin::Personal) {
            sqlx::query(
                "INSERT INTO neon_roles
                     (endpoint_id, name, scram_verifier, origin, password_enc, login, level,
                      api_owner_level, pending, inherit, connection_limit, valid_until,
                      bypass_rls, member_of, member_no_set, member_admin)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)",
            )
            .bind(ep.id)
            .bind(copy.name)
            .bind(copy.verifier)
            .bind(copy.origin.as_str())
            .bind(copy.password_enc)
            .bind(copy.login)
            .bind(copy.level.as_str())
            .bind(copy.api_owner_level)
            .bind(copy.pending)
            .bind(copy.attributes.inherit)
            .bind(copy.attributes.connection_limit)
            .bind(copy.attributes.valid_until)
            .bind(copy.attributes.bypass_rls)
            .bind(&copy.attributes.member_of)
            .bind(&copy.attributes.member_no_set)
            .bind(&copy.attributes.member_admin)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("INSERT INTO neon_databases (endpoint_id, name, owner) VALUES ($1, $2, $3)")
            .bind(ep.id)
            .bind(ep.database_name)
            .bind(ep.owner_role)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO branch_databases
               (branch_id, name, owner, origin, data_api, data_api_bootstrapped, default_schema)
             VALUES ($1, $2, $3, 'main', 'on', true,
                     coalesce((SELECT pd.default_schema
                               FROM branches c
                               JOIN branch_databases pd ON pd.branch_id = c.parent_branch_id
                                                       AND pd.origin = 'main'
                               WHERE c.id = $1), 'public'))",
        )
        .bind(id.as_uuid())
        .bind(ep.database_name)
        .bind(ep.owner_role)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE branches SET endpoint_id = $2, timeline_id = $3, updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .bind(ep.id)
        .bind(timeline_id)
        .execute(&mut *tx)
        .await?;
        if database_id == id.as_uuid() {
            sqlx::query(
                "UPDATE databases SET database_name = $2, owner_role = $3, updated_at = now()
                 WHERE id = $1",
            )
            .bind(database_id)
            .bind(ep.database_name)
            .bind(ep.owner_role)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Of `ids`, the branches that have a `branch_databases` row of origin `main`, in one query.
    pub async fn branches_with_main_database(
        &self,
        ids: &[BranchId],
    ) -> Result<HashSet<BranchId>, MetadataError> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let uuids: Vec<Uuid> = ids.iter().map(|b| b.as_uuid()).collect();
        let found: Vec<Uuid> = sqlx::query_scalar(
            "SELECT DISTINCT branch_id FROM branch_databases
             WHERE branch_id = ANY($1) AND origin = 'main'",
        )
        .bind(&uuids)
        .fetch_all(&self.pool)
        .await?;
        Ok(found.into_iter().map(BranchId).collect())
    }

    /// ready + sealed password + branched_at_lsn + history_start (`created_at` when `None`),
    /// only from provisioning; for main also the database row (ruling 4). False (nothing
    /// written) otherwise, or when main's database is being deleted.
    pub async fn set_branch_ready(
        &self,
        id: BranchId,
        sealed_password: &str,
        branched_at_lsn: Option<&str>,
        history_start: Option<DateTime<Utc>>,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_database_of_main(&mut tx, id).await?;
        let database_id: Option<Uuid> = sqlx::query_scalar(
            "UPDATE branches
             SET status = 'ready', owner_password_enc = $2, branched_at_lsn = $3, error = NULL,
                 history_start = COALESCE($4, created_at), updated_at = now()
             WHERE id = $1 AND status = 'provisioning'
             RETURNING database_id",
        )
        .bind(id.as_uuid())
        .bind(sealed_password)
        .bind(branched_at_lsn)
        .bind(history_start)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(database_id) = database_id else {
            return Ok(false);
        };
        if database_id == id.as_uuid() {
            let database = sqlx::query(
                "UPDATE databases SET status = 'ready', error = NULL, updated_at = now()
                 WHERE id = $1 AND status NOT IN ('deleting', 'deleted')",
            )
            .bind(database_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if database != 1 {
                return Ok(false);
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Records (or clears) the timeline a reset or restore is building, and when. False once a
    /// delete has begun.
    pub async fn set_pending_timeline(
        &self,
        id: BranchId,
        timeline_id: Option<&str>,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE branches
             SET pending_timeline_id = $2,
                 pending_timeline_at = CASE WHEN $2::text IS NULL THEN NULL ELSE now() END,
                 updated_at = now()
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(id.as_uuid())
        .bind(timeline_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// One transaction (reset): branch.timeline_id and its endpoint's timeline_id := pending;
    /// superseded := old; pending := NULL; history_start := `history_start` (now when `None`:
    /// the new timeline starts at the parent's head); branched_at_lsn := `branched_at_lsn`
    /// (the LSN of the parent the new timeline starts at; it is the base location of the
    /// branch's schema). Only while resetting and while the pending
    /// timeline is `expected_pending` (the one the caller created). False otherwise, with
    /// nothing written.
    pub async fn switch_to_pending_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        history_start: Option<DateTime<Utc>>,
        branched_at_lsn: &str,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_database_of_branch(&mut tx, id).await?;
        let switched: Option<(Option<String>, String)> = sqlx::query_as(
            "UPDATE branches
             SET superseded_timeline_id = timeline_id, timeline_id = pending_timeline_id,
                 pending_timeline_id = NULL, pending_timeline_at = NULL,
                 history_start = COALESCE($3, now()), branched_at_lsn = $4, updated_at = now()
             WHERE id = $1 AND status = 'resetting' AND pending_timeline_id = $2
             RETURNING endpoint_id, timeline_id",
        )
        .bind(id.as_uuid())
        .bind(expected_pending)
        .bind(history_start)
        .bind(branched_at_lsn)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((endpoint_id, timeline_id)) = switched else {
            return Ok(false);
        };
        move_endpoint(&mut tx, endpoint_id.as_deref(), &timeline_id).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// One transaction (restore): insert the backup (kind backup, status ready, no endpoint,
    /// old timeline and its history_start, parent = id, name given); switch as above but
    /// without superseded; the branch keeps its `history_start` (Neon copies the old layers into
    /// the restored timeline, so earlier points stay reachable). Only
    /// while resetting and while the pending timeline is `expected_pending` (the one the
    /// caller created); false otherwise. [`MetadataError::BranchNameTaken`] (nothing written)
    /// when a live branch already has `backup_name`.
    ///
    /// Step 6g (E18): in the same transaction, the branch's schema bases that were current on
    /// the old timeline at `restore_lsn` (the point the restored timeline was branched at) are
    /// appended again, positioned at the restored timeline's start, so they're current after
    /// the switch.
    pub async fn switch_to_restored_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        backup: BranchId,
        backup_name: &str,
        restore_lsn: &str,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_database_of_branch(&mut tx, id).await?;
        let current: Option<(Uuid, Option<String>, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT database_id, timeline_id, history_start FROM branches
             WHERE id = $1 AND status = 'resetting' AND pending_timeline_id = $2
             FOR UPDATE",
        )
        .bind(id.as_uuid())
        .bind(expected_pending)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((database_id, old_timeline, old_history_start)) = current else {
            return Ok(false);
        };
        sqlx::query(
            "INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status,
                                   timeline_id, history_start, guard_logins)
             VALUES ($1, $2, $3, $4, 'backup', 'ready', $5, $6,
                     (SELECT guard_logins FROM branches WHERE id = $4))",
        )
        .bind(backup.as_uuid())
        .bind(database_id)
        .bind(backup_name)
        .bind(id.as_uuid())
        .bind(&old_timeline)
        .bind(old_history_start)
        .execute(&mut *tx)
        .await
        .map_err(|err| match err {
            // Nothing was written; the caller treats it as a failure before the switch.
            sqlx::Error::Database(e) if e.constraint() == Some("branches_live_name") => {
                MetadataError::BranchNameTaken(backup_name.to_string())
            }
            err => err.into(),
        })?;
        copy_guard_exemptions(&mut tx, id.as_uuid(), backup.as_uuid()).await?;
        let (endpoint_id, timeline_id): (Option<String>, String) = sqlx::query_as(
            "UPDATE branches
             SET timeline_id = pending_timeline_id, pending_timeline_id = NULL,
                 pending_timeline_at = NULL, updated_at = now()
             WHERE id = $1
             RETURNING endpoint_id, timeline_id",
        )
        .bind(id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        move_endpoint(&mut tx, endpoint_id.as_deref(), &timeline_id).await?;
        if let Some(old_timeline) = old_timeline.as_deref() {
            crate::schema_bases::copy_bases_for_restore(
                &mut tx,
                id,
                old_timeline,
                restore_lsn,
                &timeline_id,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Compare-and-swap on `pending_timeline_id`: `expected` → `new`, only while the branch
    /// isn't deleting or deleted. True when it changed. `pending_timeline_at` follows: cleared
    /// with it, or the time of this write. One statement on the branch row, so it takes no
    /// other lock (see the lock order above).
    ///
    /// A run clears a pending timeline this way *before* deleting it: when the swap fails,
    /// another run (or a delete) owns that timeline, and this run leaves it alone.
    pub async fn swap_pending_timeline(
        &self,
        id: BranchId,
        expected: Option<&str>,
        new: Option<&str>,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE branches
             SET pending_timeline_id = $3,
                 pending_timeline_at = CASE WHEN $3::text IS NULL THEN NULL ELSE now() END,
                 updated_at = now()
             WHERE id = $1 AND pending_timeline_id IS NOT DISTINCT FROM $2
               AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(id.as_uuid())
        .bind(expected)
        .bind(new)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// resetting → resetting (refreshing `updated_at`), only when the row hasn't changed for
    /// `max_age_secs`: reconcile's claim on a reset or restore left behind. Of two runs that
    /// both saw it stale, only the first wins. Main is included. True when it claimed it.
    pub async fn claim_stale_reset(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        let claimed = sqlx::query(
            "UPDATE branches SET updated_at = now()
             WHERE id = $1 AND status = 'resetting'
               AND updated_at < now() - make_interval(secs => $2)",
        )
        .bind(id.as_uuid())
        .bind(max_age_secs as f64)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(claimed == 1)
    }

    /// resetting → failed with `error`, only when its pending timeline was recorded more than
    /// `max_age_secs` ago: a restore whose detach stayed unresolved. The pending timeline stays
    /// recorded (a detach may still be running on it). Main included; one statement on the
    /// branch row, so it takes no other lock. True when it failed the branch.
    pub async fn fail_stuck_restore(
        &self,
        id: BranchId,
        max_age_secs: i64,
        error: &str,
    ) -> Result<bool, MetadataError> {
        let failed = sqlx::query(
            "UPDATE branches SET status = 'failed', error = $3, updated_at = now()
             WHERE id = $1 AND status = 'resetting' AND pending_timeline_id IS NOT NULL
               AND pending_timeline_at < now() - make_interval(secs => $2)",
        )
        .bind(id.as_uuid())
        .bind(max_age_secs as f64)
        .bind(error)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(failed == 1)
    }

    /// pending/provisioning → failed ("provisioning timed out"), only when the row hasn't
    /// changed for `max_age_secs`. Never main: its database's `fail_if_stale` owns it. True
    /// when it failed the branch.
    pub async fn fail_branch_if_stale(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        let failed = sqlx::query(
            "UPDATE branches SET status = 'failed', error = 'provisioning timed out',
                                 updated_at = now()
             WHERE id = $1 AND parent_branch_id IS NOT NULL
               AND status IN ('pending', 'provisioning')
               AND updated_at < now() - make_interval(secs => $2)",
        )
        .bind(id.as_uuid())
        .bind(max_age_secs as f64)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(failed == 1)
    }

    pub async fn clear_superseded_timeline(&self, id: BranchId) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE branches SET superseded_timeline_id = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(id.as_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn database_settings(
        &self,
        db: DatabaseId,
    ) -> Result<Option<DatabaseSettings>, MetadataError> {
        let row: Option<(i64, i32)> = sqlx::query_as(
            "SELECT EXTRACT(EPOCH FROM history_window)::bigint, max_branches
             FROM databases WHERE id = $1",
        )
        .bind(db.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(secs, max_branches)| DatabaseSettings {
            history_window: Duration::from_secs(secs.max(0) as u64),
            max_branches,
        }))
    }

    /// Changes the given settings of a live database; `None` keeps one. False once a delete
    /// has begun, or for a missing database.
    pub async fn update_database_settings(
        &self,
        db: DatabaseId,
        patch: &SettingsPatch,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE databases
             SET history_window = CASE WHEN $2::float8 IS NULL THEN history_window
                                       ELSE make_interval(secs => $2::float8) END,
                 max_branches = COALESCE($3, max_branches),
                 max_rows = COALESCE($4, max_rows),
                 statement_timeout_ms = COALESCE($5, statement_timeout_ms),
                 max_connections = COALESCE($6, max_connections),
                 storage_max_connections = COALESCE($7, storage_max_connections),
                 updated_at = now()
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(db.as_uuid())
        .bind(patch.history_window.map(|w| w.as_secs_f64()))
        .bind(patch.max_branches)
        .bind(patch.max_rows)
        .bind(patch.statement_timeout_ms)
        .bind(patch.max_connections)
        .bind(patch.storage_max_connections)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Records the Neon tenant every branch of `db` lives in. False once a delete has begun.
    pub async fn set_neon_tenant(
        &self,
        db: DatabaseId,
        tenant: &str,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE databases SET neon_tenant_id = $2
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(db.as_uuid())
        .bind(tenant)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Every branch that isn't deleted, with the age of its last change.
    pub async fn list_live_branches_for_reconcile(&self) -> Result<Vec<LiveBranch>, MetadataError> {
        sqlx::query(
            "SELECT id, database_id, kind, status, endpoint_id, timeline_id, pending_timeline_id,
                    superseded_timeline_id,
                    EXTRACT(EPOCH FROM now() - updated_at)::bigint AS age_secs
             FROM branches
             WHERE status <> 'deleted'
             ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            Ok(LiveBranch {
                id: BranchId(r.get("id")),
                database_id: DatabaseId(r.get("database_id")),
                kind: BranchKind::parse(r.get("kind"))?,
                status: BranchStatus::parse(r.get("status"))?,
                endpoint_id: r.get("endpoint_id"),
                timeline_id: r.get("timeline_id"),
                pending_timeline_id: r.get("pending_timeline_id"),
                superseded_timeline_id: r.get("superseded_timeline_id"),
                age_secs: r.get("age_secs"),
            })
        })
        .collect()
    }
}

/// For main (whose id is its database's), locks the database row: the lock order puts it
/// before main's branch row. Other branches' writes here never touch `databases`, so nothing
/// is locked for them. A branch's `database_id` never changes, so reading it unlocked is safe.
async fn lock_database_of_main(
    tx: &mut Transaction<'static, Postgres>,
    id: BranchId,
) -> Result<(), MetadataError> {
    sqlx::query(
        "SELECT 1 FROM databases
         WHERE id = (SELECT database_id FROM branches WHERE id = $1) AND id = $1
         FOR NO KEY UPDATE",
    )
    .bind(id.as_uuid())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Locks the database row of branch `id` (`FOR NO KEY UPDATE`), before any branch row: the
/// lock order above.
pub(crate) async fn lock_database_of_branch(
    tx: &mut Transaction<'static, Postgres>,
    id: BranchId,
) -> Result<(), MetadataError> {
    sqlx::query(
        "SELECT 1 FROM databases
         WHERE id = (SELECT database_id FROM branches WHERE id = $1)
         FOR NO KEY UPDATE",
    )
    .bind(id.as_uuid())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Points the branch's endpoint (if it has one) at its new timeline; the next compute's spec
/// reads it from there.
async fn move_endpoint(
    tx: &mut Transaction<'static, Postgres>,
    endpoint_id: Option<&str>,
    timeline_id: &str,
) -> Result<(), MetadataError> {
    if let Some(endpoint_id) = endpoint_id {
        sqlx::query("UPDATE neon_endpoints SET timeline_id = $2 WHERE id = $1")
            .bind(endpoint_id)
            .bind(timeline_id)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}
