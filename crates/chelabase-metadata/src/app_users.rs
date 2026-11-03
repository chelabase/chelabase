//! App users: per-project auth settings (`project_auth`), the per-branch sync-role password
//! and app sessions with rotating refresh tokens.
//!
//! Lock order: the `projects` row first, then `project_auth`. Session writes lock one
//! `app_sessions` row.

use std::time::Duration;

use chelabase_types::{BranchId, DatabaseId, ProjectId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::{MetadataClient, MetadataError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum AppUsersStatus {
    Enabling,
    Enabled,
    Disabling,
    Failed,
}

impl AppUsersStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabling => "enabling",
            Self::Enabled => "enabled",
            Self::Disabling => "disabling",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "enabling" => Ok(Self::Enabling),
            "enabled" => Ok(Self::Enabled),
            "disabling" => Ok(Self::Disabling),
            "failed" => Ok(Self::Failed),
            other => Err(MetadataError::InvalidStatus(other.to_string())),
        }
    }
}

/// What deleting an app user does to their row in the users table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum OnDelete {
    Delete,
    Mark,
}

impl OnDelete {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Mark => "mark",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "delete" => Ok(Self::Delete),
            "mark" => Ok(Self::Mark),
            other => Err(MetadataError::InvalidStatus(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectAuth {
    pub project_id: ProjectId,
    pub status: AppUsersStatus,
    pub error: Option<String>,
    pub database_id: DatabaseId,
    pub pg_database: String,
    pub schema_name: String,
    pub table_name: String,
    pub email_verification: bool,
    pub on_delete: OnDelete,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewProjectAuth<'a> {
    pub project_id: ProjectId,
    pub database_id: DatabaseId,
    pub pg_database: &'a str,
    pub schema_name: &'a str,
    pub table_name: &'a str,
    pub email_verification: bool,
    pub on_delete: OnDelete,
}

/// How [`MetadataClient::begin_enable_app_users`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginEnable {
    /// A new `enabling` row.
    Started,
    /// A `failed` row with the same location went back to `enabling`.
    Retry,
    /// A row exists (any other state, or another location): nothing written.
    Exists(ProjectAuth),
}

#[derive(Clone, PartialEq, Eq)]
pub struct BranchAuthSync {
    pub branch_id: BranchId,
    pub pg_database: String,
    pub password_sealed: String,
}

impl std::fmt::Debug for BranchAuthSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchAuthSync")
            .field("branch_id", &self.branch_id)
            .field("pg_database", &self.pg_database)
            .field("password_sealed", &"<redacted>")
            .finish()
    }
}

/// An app session without its token hashes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppSession {
    pub id: Uuid,
    pub project_id: ProjectId,
    pub user_id: Uuid,
    pub identity_session_id: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Clone)]
pub struct NewAppSession<'a> {
    pub id: Uuid,
    pub project_id: ProjectId,
    pub user_id: Uuid,
    pub identity_session_id: &'a str,
    pub refresh_hash: [u8; 32],
    pub expires_at: DateTime<Utc>,
}

impl std::fmt::Debug for NewAppSession<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewAppSession")
            .field("id", &self.id)
            .field("project_id", &self.project_id)
            .field("user_id", &self.user_id)
            .field("identity_session_id", &self.identity_session_id)
            .field("refresh_hash", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// What a refresh stores when the presented token is the current one.
#[derive(Clone)]
pub struct RefreshNext {
    pub new_hash: [u8; 32],
    /// The new token, sealed, handed to a client that retries the old one within `grace`.
    pub new_sealed: String,
    /// The session now lasts this much longer…
    pub sliding: Duration,
    /// …but never past this long after it was created.
    pub cap: Duration,
    /// How long the previous token still gets `new_sealed`.
    pub grace: Duration,
}

impl std::fmt::Debug for RefreshNext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RefreshNext")
            .field("new_hash", &"<redacted>")
            .field("new_sealed", &"<redacted>")
            .field("sliding", &self.sliding)
            .field("cap", &self.cap)
            .field("grace", &self.grace)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// The current token: replaced.
    Rotated(AppSession),
    /// The previous token, within its grace: the session didn't change; `sealed` is the current
    /// token, sealed.
    Grace { session: AppSession, sealed: String },
    /// The previous token after its grace: the session is now revoked.
    Reused(AppSession),
    /// Unknown session, another project's, revoked, expired or a token that matches neither.
    Invalid,
}

/// What presenting a refresh token would do, read without locking or writing anything
/// ([`MetadataClient::peek_app_session`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeekOutcome {
    /// The current token (`grace: false`), or the previous one within its grace
    /// (`grace: true`): a refresh would answer. A grace token's refresh writes nothing, but
    /// it is reuse once the grace ends, so a caller acts on it before the grace runs out.
    Live { session: AppSession, grace: bool },
    /// The previous token after its grace: a refresh would revoke the session.
    Reused,
    /// Unknown session, another project's, revoked, expired or a token that matches neither.
    Invalid,
}

fn auth_from_row(r: &PgRow) -> Result<ProjectAuth, MetadataError> {
    Ok(ProjectAuth {
        project_id: ProjectId(r.get("project_id")),
        status: AppUsersStatus::parse(r.get("status"))?,
        error: r.get("error"),
        database_id: DatabaseId(r.get("database_id")),
        pg_database: r.get("pg_database"),
        schema_name: r.get("schema_name"),
        table_name: r.get("table_name"),
        email_verification: r.get("email_verification"),
        on_delete: OnDelete::parse(r.get("on_delete"))?,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

fn session_from_row(r: &PgRow) -> AppSession {
    AppSession {
        id: r.get("id"),
        project_id: ProjectId(r.get("project_id")),
        user_id: r.get("user_id"),
        identity_session_id: r.get("identity_session_id"),
        created_at: r.get("created_at"),
        last_used_at: r.get("last_used_at"),
        expires_at: r.get("expires_at"),
        revoked_at: r.get("revoked_at"),
    }
}

impl MetadataClient {
    pub async fn project_auth(&self, project: ProjectId) -> Result<Option<ProjectAuth>, MetadataError> {
        sqlx::query(
            "SELECT project_id, status, error, database_id, pg_database,
                    schema_name, table_name, email_verification, on_delete, created_at, updated_at
             FROM project_auth WHERE project_id = $1",
        )
        .bind(project.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(auth_from_row)
        .transpose()
    }

    /// The app-users status of each of the tenant's projects that has a `project_auth` row,
    /// in one query.
    pub async fn project_auth_statuses(
        &self,
        tenant: chelabase_types::TenantId,
    ) -> Result<std::collections::HashMap<ProjectId, AppUsersStatus>, MetadataError> {
        sqlx::query(
            "SELECT a.project_id, a.status FROM project_auth a
             JOIN projects p ON p.id = a.project_id
             WHERE p.tenant_id = $1 AND p.status <> 'deleted'",
        )
        .bind(tenant.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| Ok((ProjectId(r.get("project_id")), AppUsersStatus::parse(r.get("status"))?)))
        .collect()
    }

    /// The `project_auth` row whose auth database is `db`, if any.
    pub async fn app_auth_for_database(&self, db: DatabaseId) -> Result<Option<ProjectAuth>, MetadataError> {
        sqlx::query(
            "SELECT project_id, status, error, database_id, pg_database,
                    schema_name, table_name, email_verification, on_delete, created_at, updated_at
             FROM project_auth WHERE database_id = $1",
        )
        .bind(db.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(auth_from_row)
        .transpose()
    }

    /// Starts enabling app users. Locks the project row (`FOR UPDATE`) and refuses with
    /// [`MetadataError::ProjectDeleting`] unless the project is `active`. A missing row is
    /// inserted as `enabling`; a `failed` row with the same database, schema and table goes
    /// back to `enabling` with its error cleared and the two settings updated; any other row is
    /// returned untouched.
    pub async fn begin_enable_app_users(
        &self,
        new: &NewProjectAuth<'_>,
    ) -> Result<BeginEnable, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let status: Option<String> =
            sqlx::query_scalar("SELECT status FROM projects WHERE id = $1 FOR UPDATE")
                .bind(new.project_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        match status.as_deref() {
            None => return Err(MetadataError::ProjectNotFound(new.project_id)),
            Some("active") => {}
            Some(_) => return Err(MetadataError::ProjectDeleting(new.project_id)),
        }
        let existing = sqlx::query(
            "SELECT project_id, status, error, database_id, pg_database,
                    schema_name, table_name, email_verification, on_delete, created_at, updated_at
             FROM project_auth WHERE project_id = $1 FOR UPDATE",
        )
        .bind(new.project_id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        let outcome = match existing {
            None => {
                sqlx::query(
                    "INSERT INTO project_auth (project_id, status, database_id, pg_database,
                         schema_name, table_name, email_verification, on_delete)
                     VALUES ($1, 'enabling', $2, $3, $4, $5, $6, $7)",
                )
                .bind(new.project_id.as_uuid())
                .bind(new.database_id.as_uuid())
                .bind(new.pg_database)
                .bind(new.schema_name)
                .bind(new.table_name)
                .bind(new.email_verification)
                .bind(new.on_delete.as_str())
                .execute(&mut *tx)
                .await?;
                BeginEnable::Started
            }
            Some(row) => {
                let row = auth_from_row(&row)?;
                let same_place = row.database_id == new.database_id
                    && row.pg_database == new.pg_database
                    && row.schema_name == new.schema_name
                    && row.table_name == new.table_name;
                if row.status == AppUsersStatus::Failed && same_place {
                    sqlx::query(
                        "UPDATE project_auth
                         SET status = 'enabling', error = NULL, email_verification = $2,
                             on_delete = $3, updated_at = now()
                         WHERE project_id = $1",
                    )
                    .bind(new.project_id.as_uuid())
                    .bind(new.email_verification)
                    .bind(new.on_delete.as_str())
                    .execute(&mut *tx)
                    .await?;
                    BeginEnable::Retry
                } else {
                    BeginEnable::Exists(row)
                }
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// Changes the given settings of a project's app users; the row, or `None` without one.
    pub async fn update_app_users_settings(
        &self,
        project: ProjectId,
        email_verification: Option<bool>,
        on_delete: Option<OnDelete>,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        sqlx::query(
            "UPDATE project_auth
             SET email_verification = COALESCE($2, email_verification),
                 on_delete = COALESCE($3, on_delete), updated_at = now()
             WHERE project_id = $1
             RETURNING project_id, status, error, database_id, pg_database,
                       schema_name, table_name, email_verification, on_delete, created_at,
                       updated_at",
        )
        .bind(project.as_uuid())
        .bind(email_verification)
        .bind(on_delete.map(OnDelete::as_str))
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(auth_from_row)
        .transpose()
    }

    /// Ends enabling: `enabled`, or `failed` with the error. True when the row was `enabling`.
    pub async fn finish_enable_app_users(
        &self,
        project: ProjectId,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        let (status, error) = match &outcome {
            Ok(()) => ("enabled", None),
            Err(error) => ("failed", Some(error.as_str())),
        };
        let updated = sqlx::query(
            "UPDATE project_auth SET status = $2, error = $3, updated_at = now()
             WHERE project_id = $1 AND status = 'enabling'",
        )
        .bind(project.as_uuid())
        .bind(status)
        .bind(error)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Moves `enabled` or `failed` to `disabling`. True when it moved.
    pub async fn begin_disable_app_users(&self, project: ProjectId) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE project_auth SET status = 'disabling', updated_at = now()
             WHERE project_id = $1 AND status IN ('enabled', 'failed')",
        )
        .bind(project.as_uuid())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Deletes the row whatever its status: the project's delete, which then drops the pool.
    pub async fn delete_project_auth(&self, project: ProjectId) -> Result<(), MetadataError> {
        sqlx::query("DELETE FROM project_auth WHERE project_id = $1")
            .bind(project.as_uuid())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The end of a disable: deletes the row only while it's `disabling`, so a duplicate job
    /// can't take the row of an enable that started since. True when it did.
    pub async fn finish_disable_app_users(&self, project: ProjectId) -> Result<bool, MetadataError> {
        let deleted = sqlx::query("DELETE FROM project_auth WHERE project_id = $1 AND status = 'disabling'")
            .bind(project.as_uuid())
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(deleted == 1)
    }

    /// Records (`Some`) or clears (`None`) the worker's last reconcile error on an `enabled`
    /// row (a pool gone from the identity service, a branch it can't upgrade), or why a
    /// `disabling` row's disable is on hold (its pool still has users). Not an `enabling` row
    /// (an enable's own outcome ends it), nor a `failed` one (it keeps the enable's error). A
    /// row already holding the value is left alone.
    pub async fn set_app_users_error(
        &self,
        project: ProjectId,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE project_auth SET error = $2, updated_at = now()
             WHERE project_id = $1 AND status IN ('enabled', 'disabling')
               AND error IS DISTINCT FROM $2",
        )
        .bind(project.as_uuid())
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn enabled_app_projects(&self) -> Result<Vec<ProjectAuth>, MetadataError> {
        sqlx::query(
            "SELECT project_id, status, error, database_id, pg_database,
                    schema_name, table_name, email_verification, on_delete, created_at, updated_at
             FROM project_auth WHERE status = 'enabled' ORDER BY created_at, project_id",
        )
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(auth_from_row)
        .collect()
    }

    /// Rows `enabling` or `disabling` and untouched for `older_than`.
    pub async fn app_users_in_transition(
        &self,
        older_than: Duration,
    ) -> Result<Vec<ProjectAuth>, MetadataError> {
        sqlx::query(
            "SELECT project_id, status, error, database_id, pg_database,
                    schema_name, table_name, email_verification, on_delete, created_at, updated_at
             FROM project_auth
             WHERE status IN ('enabling', 'disabling')
               AND updated_at < now() - make_interval(secs => $1)
             ORDER BY updated_at, project_id",
        )
        .bind(older_than.as_secs_f64())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(auth_from_row)
        .collect()
    }

    pub async fn branch_auth_sync(&self, branch: BranchId) -> Result<Option<BranchAuthSync>, MetadataError> {
        let row = sqlx::query(
            "SELECT branch_id, pg_database, password_sealed FROM branch_auth_sync WHERE branch_id = $1",
        )
        .bind(branch.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| BranchAuthSync {
            branch_id: BranchId(r.get("branch_id")),
            pg_database: r.get("pg_database"),
            password_sealed: r.get("password_sealed"),
        }))
    }

    pub async fn set_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "INSERT INTO branch_auth_sync (branch_id, pg_database, password_sealed)
             VALUES ($1, $2, $3)
             ON CONFLICT (branch_id) DO UPDATE
             SET pg_database = EXCLUDED.pg_database,
                 password_sealed = EXCLUDED.password_sealed, updated_at = now()",
        )
        .bind(branch.as_uuid())
        .bind(pg_database)
        .bind(sealed)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stores `sealed` as the branch's sync password unless the row already holds one made for
    /// the same Postgres database; returns the stored one (two bootstraps of one branch racing
    /// end up with the same password). A row for another database is replaced.
    pub async fn claim_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> Result<String, MetadataError> {
        loop {
            let stored: Option<String> = sqlx::query_scalar(
                "INSERT INTO branch_auth_sync (branch_id, pg_database, password_sealed)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (branch_id) DO UPDATE
                 SET pg_database = EXCLUDED.pg_database,
                     password_sealed = EXCLUDED.password_sealed, updated_at = now()
                 WHERE branch_auth_sync.pg_database <> EXCLUDED.pg_database
                 RETURNING password_sealed",
            )
            .bind(branch.as_uuid())
            .bind(pg_database)
            .bind(sealed)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(stored) = stored {
                return Ok(stored);
            }
            // The row is there for this database: read it in a new statement (a fresh snapshot,
            // so a concurrent claim's commit is seen).
            let kept: Option<String> = sqlx::query_scalar(
                "SELECT password_sealed FROM branch_auth_sync WHERE branch_id = $1 AND pg_database = $2",
            )
            .bind(branch.as_uuid())
            .bind(pg_database)
            .fetch_optional(&self.pool)
            .await?;
            if let Some(kept) = kept {
                return Ok(kept);
            }
            // Deleted or moved to another database in between: claim again.
        }
    }

    /// Replaces the branch's sync password for `pg_database` only while it still holds
    /// `expected` (compare-and-swap); returns the stored one. When another writer replaced it
    /// first, theirs comes back; with no row for `pg_database`, this is
    /// [`MetadataClient::claim_branch_auth_sync`].
    pub async fn swap_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        expected: &str,
        sealed: &str,
    ) -> Result<String, MetadataError> {
        let swapped: Option<String> = sqlx::query_scalar(
            "UPDATE branch_auth_sync SET password_sealed = $4, updated_at = now()
             WHERE branch_id = $1 AND pg_database = $2 AND password_sealed = $3
             RETURNING password_sealed",
        )
        .bind(branch.as_uuid())
        .bind(pg_database)
        .bind(expected)
        .bind(sealed)
        .fetch_optional(&self.pool)
        .await?;
        match swapped {
            Some(stored) => Ok(stored),
            None => self.claim_branch_auth_sync(branch, pg_database, sealed).await,
        }
    }

    pub async fn insert_app_session(&self, s: &NewAppSession<'_>) -> Result<(), MetadataError> {
        sqlx::query(
            "INSERT INTO app_sessions (id, project_id, user_id, identity_session_id, refresh_hash,
                 expires_at)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(s.id)
        .bind(s.project_id.as_uuid())
        .bind(s.user_id)
        .bind(s.identity_session_id)
        .bind(s.refresh_hash.as_slice())
        .bind(s.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The session, revoked or not.
    pub async fn app_session(&self, id: Uuid) -> Result<Option<AppSession>, MetadataError> {
        Ok(sqlx::query(
            "SELECT id, project_id, user_id, identity_session_id, created_at, last_used_at,
                    expires_at, revoked_at
             FROM app_sessions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(session_from_row))
    }

    /// Presents a refresh token for `id`, in one transaction holding the session row.
    ///
    /// Unknown, another project's or revoked sessions are `Invalid` and nothing is written. A
    /// session past its expiry or its `cap` is revoked and `Invalid`. The current token rotates
    /// (`Rotated`); the previous one gets the new sealed token within the grace (`Grace`, no
    /// change) and revokes the session after it (`Reused`); any other token is `Invalid` and
    /// changes nothing.
    pub async fn refresh_app_session(
        &self,
        id: Uuid,
        project: ProjectId,
        presented: [u8; 32],
        next: &RefreshNext,
    ) -> Result<RefreshOutcome, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT id, project_id, user_id, identity_session_id, refresh_hash, prev_refresh_hash,
                    grace_refresh_sealed, grace_until, created_at, last_used_at, expires_at,
                    revoked_at, now() AS now
             FROM app_sessions WHERE id = $1 FOR UPDATE",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            return Ok(RefreshOutcome::Invalid);
        };
        let session = session_from_row(&row);
        if session.project_id != project || session.revoked_at.is_some() {
            return Ok(RefreshOutcome::Invalid);
        }
        let now: DateTime<Utc> = row.get("now");
        let cap_end = chrono::Duration::from_std(next.cap)
            .ok()
            .and_then(|cap| session.created_at.checked_add_signed(cap));
        if now >= session.expires_at || cap_end.is_some_and(|end| now >= end) {
            sqlx::query("UPDATE app_sessions SET revoked_at = now() WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(RefreshOutcome::Invalid);
        }
        let current: Vec<u8> = row.get("refresh_hash");
        let previous: Option<Vec<u8>> = row.get("prev_refresh_hash");
        let outcome = if current.as_slice() == presented.as_slice() {
            let rotated = sqlx::query(
                "UPDATE app_sessions
                 SET prev_refresh_hash = refresh_hash, refresh_hash = $2,
                     grace_refresh_sealed = $3, grace_until = now() + make_interval(secs => $4),
                     last_used_at = now(),
                     expires_at = LEAST(now() + make_interval(secs => $5),
                                        created_at + make_interval(secs => $6))
                 WHERE id = $1
                 RETURNING id, project_id, user_id, identity_session_id, created_at,
                           last_used_at, expires_at, revoked_at",
            )
            .bind(id)
            .bind(next.new_hash.as_slice())
            .bind(&next.new_sealed)
            .bind(next.grace.as_secs_f64())
            .bind(next.sliding.as_secs_f64())
            .bind(next.cap.as_secs_f64())
            .fetch_one(&mut *tx)
            .await?;
            RefreshOutcome::Rotated(session_from_row(&rotated))
        } else if previous.as_deref() == Some(presented.as_slice()) {
            let grace_until: Option<DateTime<Utc>> = row.get("grace_until");
            let sealed: Option<String> = row.get("grace_refresh_sealed");
            match (grace_until, sealed) {
                (Some(until), Some(sealed)) if until > now => RefreshOutcome::Grace { session, sealed },
                _ => {
                    let revoked = sqlx::query(
                        "UPDATE app_sessions SET revoked_at = now() WHERE id = $1
                         RETURNING id, project_id, user_id, identity_session_id, created_at,
                                   last_used_at, expires_at, revoked_at",
                    )
                    .bind(id)
                    .fetch_one(&mut *tx)
                    .await?;
                    RefreshOutcome::Reused(session_from_row(&revoked))
                }
            }
        } else {
            RefreshOutcome::Invalid
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// What [`MetadataClient::refresh_app_session`] would do with `presented`, by the same
    /// rules (project, revocation, expiry and `cap`, current token, previous token and grace),
    /// read in one statement: no lock, no rotation and no revocation. A session past its expiry
    /// or `cap` is `Invalid` but left for `refresh_app_session` to revoke; the previous token
    /// after its grace is `Reused` and the session stays live.
    pub async fn peek_app_session(
        &self,
        id: Uuid,
        project: ProjectId,
        presented: [u8; 32],
        cap: Duration,
    ) -> Result<PeekOutcome, MetadataError> {
        let row = sqlx::query(
            "SELECT id, project_id, user_id, identity_session_id, refresh_hash, prev_refresh_hash,
                    grace_until, grace_refresh_sealed IS NOT NULL AS has_sealed, created_at,
                    last_used_at, expires_at, revoked_at, now() AS now
             FROM app_sessions WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(PeekOutcome::Invalid);
        };
        let session = session_from_row(&row);
        if session.project_id != project || session.revoked_at.is_some() {
            return Ok(PeekOutcome::Invalid);
        }
        let now: DateTime<Utc> = row.get("now");
        let cap_end = chrono::Duration::from_std(cap)
            .ok()
            .and_then(|cap| session.created_at.checked_add_signed(cap));
        if now >= session.expires_at || cap_end.is_some_and(|end| now >= end) {
            return Ok(PeekOutcome::Invalid);
        }
        let current: Vec<u8> = row.get("refresh_hash");
        let previous: Option<Vec<u8>> = row.get("prev_refresh_hash");
        Ok(if current.as_slice() == presented.as_slice() {
            PeekOutcome::Live { session, grace: false }
        } else if previous.as_deref() == Some(presented.as_slice()) {
            // The grace answer needs the sealed token too, as in `refresh_app_session`; peek
            // only checks that it is there and never reads it.
            let grace_until: Option<DateTime<Utc>> = row.get("grace_until");
            let sealed: bool = row.get("has_sealed");
            match grace_until {
                Some(until) if until > now && sealed => PeekOutcome::Live { session, grace: true },
                _ => PeekOutcome::Reused,
            }
        } else {
            PeekOutcome::Invalid
        })
    }

    /// Revokes a live session of the project. `Some` when it was live and in that project.
    pub async fn revoke_app_session(
        &self,
        id: Uuid,
        project: ProjectId,
    ) -> Result<Option<AppSession>, MetadataError> {
        Ok(sqlx::query(
            "UPDATE app_sessions SET revoked_at = now()
             WHERE id = $1 AND project_id = $2 AND revoked_at IS NULL
             RETURNING id, project_id, user_id, identity_session_id, created_at, last_used_at,
                       expires_at, revoked_at",
        )
        .bind(id)
        .bind(project.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(session_from_row))
    }

    /// Like [`MetadataClient::revoke_app_session`], but only when `presented` is the session's
    /// current or previous refresh token hash.
    pub async fn revoke_app_session_by_hash(
        &self,
        id: Uuid,
        project: ProjectId,
        presented: [u8; 32],
    ) -> Result<Option<AppSession>, MetadataError> {
        Ok(sqlx::query(
            "UPDATE app_sessions SET revoked_at = now()
             WHERE id = $1 AND project_id = $2 AND revoked_at IS NULL
               AND (refresh_hash = $3 OR prev_refresh_hash = $3)
             RETURNING id, project_id, user_id, identity_session_id, created_at, last_used_at,
                       expires_at, revoked_at",
        )
        .bind(id)
        .bind(project.as_uuid())
        .bind(presented.as_slice())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(session_from_row))
    }

    /// Revokes the user's live sessions except `keep`, and when `created_before` is given only
    /// those created before it. Returns the revoked ones.
    pub async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<DateTime<Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        Ok(sqlx::query(
            "UPDATE app_sessions SET revoked_at = now()
             WHERE project_id = $1 AND user_id = $2 AND revoked_at IS NULL
               AND ($3::uuid IS NULL OR id <> $3)
               AND ($4::timestamptz IS NULL OR created_at < $4)
             RETURNING id, project_id, user_id, identity_session_id, created_at, last_used_at,
                       expires_at, revoked_at",
        )
        .bind(project.as_uuid())
        .bind(user)
        .bind(keep)
        .bind(created_before)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(session_from_row)
        .collect())
    }

    /// Revokes every live session of the project; returns them.
    pub async fn revoke_project_sessions(&self, project: ProjectId) -> Result<Vec<AppSession>, MetadataError> {
        Ok(sqlx::query(
            "UPDATE app_sessions SET revoked_at = now()
             WHERE project_id = $1 AND revoked_at IS NULL
             RETURNING id, project_id, user_id, identity_session_id, created_at, last_used_at,
                       expires_at, revoked_at",
        )
        .bind(project.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(session_from_row)
        .collect())
    }

    /// The user's live sessions (not revoked, not expired), oldest first.
    pub async fn list_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
    ) -> Result<Vec<AppSession>, MetadataError> {
        Ok(sqlx::query(
            "SELECT id, project_id, user_id, identity_session_id, created_at, last_used_at,
                    expires_at, revoked_at
             FROM app_sessions
             WHERE project_id = $1 AND user_id = $2 AND revoked_at IS NULL AND expires_at > now()
             ORDER BY created_at, id",
        )
        .bind(project.as_uuid())
        .bind(user)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(session_from_row)
        .collect())
    }

    /// Deletes sessions revoked or expired more than `older_than` ago; the count. Also forgets
    /// the sealed grace token of every session past its grace window (it is the current
    /// refresh token, so it mustn't outlive the window it serves).
    pub async fn purge_app_sessions(&self, older_than: Duration) -> Result<u64, MetadataError> {
        sqlx::query(
            "UPDATE app_sessions SET grace_refresh_sealed = NULL
             WHERE grace_refresh_sealed IS NOT NULL AND grace_until < now()",
        )
        .execute(&self.pool)
        .await?;
        Ok(sqlx::query(
            "DELETE FROM app_sessions
             WHERE revoked_at < now() - make_interval(secs => $1)
                OR expires_at < now() - make_interval(secs => $1)",
        )
        .bind(older_than.as_secs_f64())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}
