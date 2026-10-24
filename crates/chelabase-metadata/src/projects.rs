//! Projects as a resource: name, slug, lifecycle (`active` → `deleting` → `deleted`).
//!
//! Lock order: the `projects` row first, then its `databases` rows (`FOR NO KEY UPDATE`), then
//! branch rows (see `branches.rs`).

use std::collections::HashSet;
use std::time::Duration;

use chelabase_types::{DatabaseId, ProjectId, TenantId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::{DatabaseStatus, MetadataClient, MetadataError};

/// The unique index on a live slug per tenant.
const LIVE_SLUG: &str = "projects_live_slug";
/// Tries `create_project` makes against concurrent creators before giving up.
const SLUG_TRIES: usize = 20;
/// Longest slug (base plus any `-N` suffix).
const SLUG_MAX: usize = 40;
const NAME_MAX_CHARS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum ProjectStatus {
    Active,
    Deleting,
    Deleted,
}

impl ProjectStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "active" => Ok(Self::Active),
            "deleting" => Ok(Self::Deleting),
            "deleted" => Ok(Self::Deleted),
            other => Err(MetadataError::InvalidStatus(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectRecord {
    pub id: ProjectId,
    pub tenant_id: TenantId,
    pub name: String,
    pub slug: String,
    pub status: ProjectStatus,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// How [`MetadataClient::begin_project_delete`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginProjectDelete {
    /// The project is `deleting`; these databases (those that were `ready` or `failed`) are too
    /// and need their delete jobs.
    Started(Vec<DatabaseId>),
    /// Missing, deleted or another tenant's.
    NotFound,
    AlreadyDeleting,
    /// A live branch of this database is protected.
    Protected(DatabaseId),
    /// This database is still `pending` or `provisioning`.
    NotReady(DatabaseId),
    /// App users are being enabled or disabled.
    AppUsersBusy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameProject {
    Renamed(ProjectRecord),
    NotFound,
    Deleting,
}

/// The slug a project name starts from: lowercase, runs of anything but `[a-z0-9]` become one
/// `-`, no leading or trailing `-`, at most 40 characters; `project` when nothing is left.
pub fn slug_base(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.truncate(SLUG_MAX);
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() { "project".to_string() } else { slug.to_string() }
}

/// The name trimmed, when it is 1 to 64 characters long.
pub fn valid_project_name(name: &str) -> Option<String> {
    let name = name.trim();
    let chars = name.chars().count();
    (1..=NAME_MAX_CHARS).contains(&chars).then(|| name.to_string())
}

/// The first of `base`, `base-2`, `base-3`, … not in `taken`, at most 40 characters.
fn free_slug(base: &str, taken: &HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    for n in 2u32.. {
        let suffix = format!("-{n}");
        let keep = base.len().min(SLUG_MAX - suffix.len());
        let candidate = format!("{}{suffix}", base[..keep].trim_end_matches('-'));
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

pub(crate) fn project_from_row(r: &PgRow) -> Result<ProjectRecord, MetadataError> {
    Ok(ProjectRecord {
        id: ProjectId(r.get("id")),
        tenant_id: TenantId(r.get("tenant_id")),
        name: r.get("name"),
        slug: r.get("slug"),
        status: ProjectStatus::parse(r.get("status"))?,
        error: r.get("error"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    })
}

impl MetadataClient {
    /// The tenant's projects that aren't deleted, oldest first.
    pub async fn list_projects(&self, tenant: TenantId) -> Result<Vec<ProjectRecord>, MetadataError> {
        sqlx::query(
            "SELECT id, tenant_id, name, slug, status, error, created_at, updated_at
             FROM projects WHERE tenant_id = $1 AND status <> 'deleted'
             ORDER BY created_at, id",
        )
        .bind(tenant.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(project_from_row)
        .collect()
    }

    /// `None` for another tenant's project and for a deleted one.
    pub async fn get_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        sqlx::query(
            "SELECT id, tenant_id, name, slug, status, error, created_at, updated_at
             FROM projects WHERE id = $1 AND tenant_id = $2 AND status <> 'deleted'",
        )
        .bind(id.as_uuid())
        .bind(tenant.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(project_from_row)
        .transpose()
    }

    /// Any tenant's project that isn't deleted (the worker and internal API have no tenant).
    pub async fn project_by_id(&self, id: ProjectId) -> Result<Option<ProjectRecord>, MetadataError> {
        sqlx::query(
            "SELECT id, tenant_id, name, slug, status, error, created_at, updated_at
             FROM projects WHERE id = $1 AND status <> 'deleted'",
        )
        .bind(id.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(project_from_row)
        .transpose()
    }

    /// Creates a project with the first free slug among `base`, `base-2`, `base-3`, …
    /// ([`slug_base`] of the name). A creator that wins the same slug first makes this retry,
    /// up to 20 times; then [`MetadataError::Conflict`].
    pub async fn create_project(
        &self,
        tenant: TenantId,
        name: &str,
    ) -> Result<ProjectRecord, MetadataError> {
        let base = slug_base(name);
        for _ in 0..SLUG_TRIES {
            let taken: HashSet<String> = sqlx::query_scalar(
                "SELECT slug FROM projects WHERE tenant_id = $1 AND status <> 'deleted'",
            )
            .bind(tenant.as_uuid())
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .collect();
            let slug = free_slug(&base, &taken);
            let inserted = sqlx::query(
                "INSERT INTO projects (id, tenant_id, name, slug) VALUES ($1, $2, $3, $4)
                 RETURNING id, tenant_id, name, slug, status, error, created_at, updated_at",
            )
            .bind(ProjectId::new().as_uuid())
            .bind(tenant.as_uuid())
            .bind(name)
            .bind(&slug)
            .fetch_one(&self.pool)
            .await;
            match inserted {
                Ok(row) => return project_from_row(&row),
                Err(sqlx::Error::Database(err)) if err.constraint() == Some(LIVE_SLUG) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Err(MetadataError::Conflict)
    }

    /// Changes the name only; the slug stays.
    pub async fn rename_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
        name: &str,
    ) -> Result<RenameProject, MetadataError> {
        let renamed = sqlx::query(
            "UPDATE projects SET name = $3, updated_at = now()
             WHERE id = $1 AND tenant_id = $2 AND status = 'active'
             RETURNING id, tenant_id, name, slug, status, error, created_at, updated_at",
        )
        .bind(id.as_uuid())
        .bind(tenant.as_uuid())
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        if let Some(row) = renamed {
            return Ok(RenameProject::Renamed(project_from_row(&row)?));
        }
        Ok(match self.get_project(tenant, id).await? {
            Some(_) => RenameProject::Deleting,
            None => RenameProject::NotFound,
        })
    }

    /// Starts deleting a project, in one transaction: locks the project, then its databases and
    /// their live branches. Refuses, in this order: not found, already deleting, a database
    /// still `pending`/`provisioning`, a protected live branch, app users in transition.
    /// Otherwise the project and its `ready`/`failed` databases (with their live branches, as
    /// [`MetadataClient::begin_delete`] does) become `deleting`, and those databases' ids come
    /// back. Databases already `deleting` aren't returned: reconcile retries them.
    pub async fn begin_project_delete(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> Result<BeginProjectDelete, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let status: Option<String> = sqlx::query_scalar(
            "SELECT status FROM projects
             WHERE id = $1 AND tenant_id = $2 AND status <> 'deleted' FOR UPDATE",
        )
        .bind(id.as_uuid())
        .bind(tenant.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        match status.as_deref() {
            None => return Ok(BeginProjectDelete::NotFound),
            Some("deleting") => return Ok(BeginProjectDelete::AlreadyDeleting),
            Some(_) => {}
        }
        let databases: Vec<(uuid::Uuid, String)> = sqlx::query_as(
            "SELECT id, status FROM databases
             WHERE project_id = $1 AND status <> 'deleted' ORDER BY id FOR NO KEY UPDATE",
        )
        .bind(id.as_uuid())
        .fetch_all(&mut *tx)
        .await?;
        let mut ids = Vec::with_capacity(databases.len());
        for (db, status) in &databases {
            let status = DatabaseStatus::parse(status)?;
            if matches!(status, DatabaseStatus::Pending | DatabaseStatus::Provisioning) {
                return Ok(BeginProjectDelete::NotReady(DatabaseId(*db)));
            }
            ids.push(*db);
        }
        // Every live branch row is locked (waiting for a protect in flight), then checked.
        let branches: Vec<(uuid::Uuid, bool)> = sqlx::query_as(
            "SELECT database_id, protected FROM branches
             WHERE database_id = ANY($1) AND status NOT IN ('deleting', 'deleted')
             ORDER BY id FOR NO KEY UPDATE",
        )
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        if let Some((db, _)) = branches.iter().find(|(_, protected)| *protected) {
            return Ok(BeginProjectDelete::Protected(DatabaseId(*db)));
        }
        let auth: Option<String> =
            sqlx::query_scalar("SELECT status FROM project_auth WHERE project_id = $1 FOR UPDATE")
                .bind(id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        if matches!(auth.as_deref(), Some("enabling" | "disabling")) {
            return Ok(BeginProjectDelete::AppUsersBusy);
        }
        sqlx::query("UPDATE projects SET status = 'deleting', error = NULL, updated_at = now() WHERE id = $1")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await?;
        let started: Vec<uuid::Uuid> = sqlx::query_scalar(
            "UPDATE databases SET status = 'deleting', updated_at = now()
             WHERE project_id = $1 AND status IN ('ready', 'failed') RETURNING id",
        )
        .bind(id.as_uuid())
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE branches SET status = 'deleting', updated_at = now()
             WHERE database_id = ANY($1) AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(&started)
        .execute(&mut *tx)
        .await?;
        // EOS-6: the project is deleting, so no login on any of its databases qualifies.
        let all: Vec<uuid::Uuid> = databases.iter().map(|(db, _)| *db).collect();
        crate::personal_logins::revoke_database_logins(&mut tx, &all).await?;
        tx.commit().await?;
        Ok(BeginProjectDelete::Started(started.into_iter().map(DatabaseId).collect()))
    }

    /// The project's databases that aren't deleted, with their last error.
    pub async fn project_databases(
        &self,
        id: ProjectId,
    ) -> Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError> {
        sqlx::query(
            "SELECT id, status, error FROM databases
             WHERE project_id = $1 AND status <> 'deleted' ORDER BY created_at, id",
        )
        .bind(id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| {
            Ok((
                DatabaseId(r.get("id")),
                DatabaseStatus::parse(r.get("status"))?,
                r.get("error"),
            ))
        })
        .collect()
    }

    /// Records (or clears) why a project's delete hasn't finished.
    pub async fn set_project_error(&self, id: ProjectId, error: Option<&str>) -> Result<(), MetadataError> {
        sqlx::query("UPDATE projects SET error = $2, updated_at = now() WHERE id = $1")
            .bind(id.as_uuid())
            .bind(error)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Ends a project's delete, in one transaction: revokes its app sessions, drops its
    /// branches' sync passwords and its `project_auth`, and tombstones the project (its slug is
    /// free again).
    pub async fn finish_project_delete(&self, id: ProjectId) -> Result<(), MetadataError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT 1 FROM projects WHERE id = $1 FOR UPDATE")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE app_sessions SET revoked_at = now() WHERE project_id = $1 AND revoked_at IS NULL")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "DELETE FROM branch_auth_sync WHERE branch_id IN (
                 SELECT b.id FROM branches b JOIN databases d ON d.id = b.database_id
                 WHERE d.project_id = $1)",
        )
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
        sqlx::query("DELETE FROM project_auth WHERE project_id = $1")
            .bind(id.as_uuid())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE projects SET status = 'deleted', deleted_at = now(), error = NULL,
                    updated_at = now()
             WHERE id = $1",
        )
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
        // EOS-6: any login still live on the project's databases goes with it. The database
        // rows are locked first, so a create in flight (which locks its database `FOR SHARE`)
        // is waited for or waits.
        let databases: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM databases WHERE project_id = $1 ORDER BY id FOR NO KEY UPDATE",
        )
        .bind(id.as_uuid())
        .fetch_all(&mut *tx)
        .await?;
        crate::personal_logins::revoke_database_logins(&mut tx, &databases).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Projects whose delete has been `deleting` and untouched for `older_than`.
    pub async fn deleting_projects(&self, older_than: Duration) -> Result<Vec<ProjectId>, MetadataError> {
        let ids: Vec<uuid::Uuid> = sqlx::query_scalar(
            "SELECT id FROM projects
             WHERE status = 'deleting' AND updated_at < now() - make_interval(secs => $1)
             ORDER BY updated_at",
        )
        .bind(older_than.as_secs_f64())
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().map(ProjectId).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_base_rules() {
        assert_eq!(slug_base("My App"), "my-app");
        assert_eq!(slug_base("  Café  Münster!! "), "caf-m-nster");
        assert_eq!(slug_base("a---b__c"), "a-b-c");
        assert_eq!(slug_base("---"), "project");
        assert_eq!(slug_base(""), "project");
        let long = slug_base(&"abc ".repeat(30));
        assert!(long.len() <= 40, "{long}");
        assert!(!long.ends_with('-'), "{long}");
        assert_eq!(slug_base(&"a".repeat(41)), "a".repeat(40));
    }

    #[test]
    fn project_name_is_trimmed_and_bounded() {
        assert_eq!(valid_project_name("  My App ").as_deref(), Some("My App"));
        assert_eq!(valid_project_name("   "), None);
        assert_eq!(valid_project_name(""), None);
        assert!(valid_project_name(&"x".repeat(64)).is_some());
        assert_eq!(valid_project_name(&"x".repeat(65)), None);
        // Characters, not bytes.
        assert!(valid_project_name(&"é".repeat(64)).is_some());
    }
}
