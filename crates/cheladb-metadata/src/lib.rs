//! Metadata store: Tenant → Project → Database lookup and provisioning state.

use cheladb_types::{DatabaseId, ProjectId, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

mod neon;
pub use neon::{NeonDatabase, NeonEndpoint, NeonRole};

/// Fixed seed tenant for local demos (`slug = demo`).
pub const DEMO_TENANT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001);
/// Fixed seed project for local demos (`slug = default`).
pub const DEMO_PROJECT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0002);

/// Connection configuration for the control-plane metadata Postgres.
#[derive(Debug, Clone)]
pub struct MetadataConfig {
    pub database_url: String,
}

impl MetadataConfig {
    pub fn from_env() -> Result<Self, MetadataError> {
        let database_url = std::env::var("CHELADB_METADATA_DATABASE_URL").map_err(|_| {
            MetadataError::Config("CHELADB_METADATA_DATABASE_URL is required".into())
        })?;
        Ok(Self { database_url })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatabaseRecord {
    pub database_id: DatabaseId,
    pub project_id: ProjectId,
    pub tenant_id: TenantId,
    pub name: String,
    pub status: DatabaseStatus,
    pub container_id: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Actual Postgres database name (sanitized from `name`).
    pub database_name: Option<String>,
    /// The revealed owner role (`postgres`).
    pub owner_role: Option<String>,
    /// Owner password, sealed with `cheladb-secrets` (aad = database id).
    pub owner_password_enc: Option<String>,
    /// Platform superuser password, sealed the same way. Never revealed.
    pub platform_password_enc: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatabaseStatus {
    Pending,
    Provisioning,
    Ready,
    Failed,
    Deleting,
    Deleted,
}

impl DatabaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
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
            "failed" => Ok(Self::Failed),
            "deleting" => Ok(Self::Deleting),
            "deleted" => Ok(Self::Deleted),
            other => Err(MetadataError::InvalidStatus(other.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("metadata config error: {0}")]
    Config(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migrate error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("tenant not found: {0}")]
    TenantNotFound(TenantId),
    #[error("project not found: {0}")]
    ProjectNotFound(ProjectId),
    #[error("project {project_id} does not belong to tenant {tenant_id}")]
    ProjectTenantMismatch {
        project_id: ProjectId,
        tenant_id: TenantId,
    },
    #[error("database name already exists in project")]
    DuplicateDatabaseName,
    #[error("invalid status: {0}")]
    InvalidStatus(String),
    #[error("invalid database name: {0}")]
    InvalidDatabaseName(String),
}

/// Postgres limits identifiers to 63 bytes.
const MAX_DATABASE_NAME_BYTES: usize = 63;
/// Databases every Postgres cluster already has.
const RESERVED_DATABASE_NAMES: [&str; 3] = ["postgres", "template0", "template1"];

/// The Postgres database name for a user-facing name: `[A-Za-z0-9_]` only
/// (other characters become `_`), at most 63 bytes, and not a system database.
pub fn postgres_database_name(name: &str) -> Result<String, MetadataError> {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let cleaned = if cleaned.is_empty() { "cheladb".to_string() } else { cleaned };
    if cleaned.len() > MAX_DATABASE_NAME_BYTES {
        return Err(MetadataError::InvalidDatabaseName(format!(
            "must be at most {MAX_DATABASE_NAME_BYTES} characters"
        )));
    }
    if RESERVED_DATABASE_NAMES.contains(&cleaned.to_ascii_lowercase().as_str()) {
        return Err(MetadataError::InvalidDatabaseName(format!(
            "\"{cleaned}\" is reserved by Postgres"
        )));
    }
    Ok(cleaned)
}

/// Outcome of [`MetadataClient::ensure_key_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCheck {
    /// First run: the check value was stored.
    Recorded,
    /// The stored check value opens with the current key.
    Matches,
    /// Different key, but nothing was encrypted yet: the new key was adopted.
    Replaced,
    /// Different key while encrypted credentials exist: they can't be read.
    Mismatch { sealed_rows: i64 },
}

/// Values stored when a database becomes ready.
#[derive(Debug, Clone, Copy)]
pub struct ReadyDatabase<'a> {
    pub container_id: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub database_name: &'a str,
    pub owner_role: &'a str,
    pub owner_password_enc: &'a str,
    pub platform_password_enc: &'a str,
}

/// A database that isn't deleted, as seen by the reconcile loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDatabase {
    pub id: DatabaseId,
    pub status: DatabaseStatus,
    /// Seconds since the row last changed.
    pub age_secs: i64,
}

/// A tenant with its projects and their databases (Studio landing page).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantOverview {
    pub id: TenantId,
    pub slug: String,
    pub projects: Vec<ProjectOverview>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectOverview {
    pub id: ProjectId,
    pub slug: String,
    pub databases: Vec<DatabaseRecord>,
}

/// Attach each database to its project, keeping project and database order.
/// Databases whose project isn't listed are dropped.
pub fn group_overview(
    id: TenantId,
    slug: String,
    projects: Vec<(ProjectId, String)>,
    databases: Vec<DatabaseRecord>,
) -> TenantOverview {
    let projects = projects
        .into_iter()
        .map(|(project_id, project_slug)| ProjectOverview {
            id: project_id,
            slug: project_slug,
            databases: databases
                .iter()
                .filter(|d| d.project_id == project_id)
                .cloned()
                .collect(),
        })
        .collect();
    TenantOverview { id, slug, projects }
}

#[derive(Debug, Clone)]
pub struct MetadataClient {
    pool: PgPool,
}

impl MetadataClient {
    pub async fn connect(config: &MetadataConfig) -> Result<Self, MetadataError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(&config.database_url)
            .await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> Result<(), MetadataError> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        info!(
            demo_tenant = %DEMO_TENANT_ID,
            demo_project = %DEMO_PROJECT_ID,
            "metadata migrations applied"
        );
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn ensure_project_in_tenant(
        &self,
        tenant_id: TenantId,
        project_id: ProjectId,
    ) -> Result<(), MetadataError> {
        let tenant_exists: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM tenants WHERE id = $1")
                .bind(tenant_id.as_uuid())
                .fetch_optional(&self.pool)
                .await?;
        if tenant_exists.is_none() {
            return Err(MetadataError::TenantNotFound(tenant_id));
        }

        let project: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT id, tenant_id FROM projects WHERE id = $1")
                .bind(project_id.as_uuid())
                .fetch_optional(&self.pool)
                .await?;

        let Some((pid, project_tenant)) = project else {
            return Err(MetadataError::ProjectNotFound(project_id));
        };
        if pid != project_id.as_uuid() || project_tenant != tenant_id.as_uuid() {
            return Err(MetadataError::ProjectTenantMismatch {
                project_id,
                tenant_id,
            });
        }
        Ok(())
    }

    pub async fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> Result<(), MetadataError> {
        self.ensure_project_in_tenant(tenant_id, project_id).await?;

        let result = sqlx::query(
            r#"
            INSERT INTO databases (id, project_id, name, status)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(project_id.as_uuid())
        .bind(name)
        .bind(DatabaseStatus::Pending.as_str())
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(sqlx::Error::Database(err))
                if err.constraint() == Some("databases_live_name_key") =>
            {
                Err(MetadataError::DuplicateDatabaseName)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Update a live database's status. False (nothing written) once a delete
    /// has begun: provisioning must never overwrite `deleting`/`deleted`.
    pub async fn set_status(
        &self,
        database_id: DatabaseId,
        status: DatabaseStatus,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            r#"
            UPDATE databases
            SET status = $2, error = NULL, updated_at = now()
            WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(status.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Mark a live database ready. False (nothing written) once a delete has
    /// begun; the caller must then tear down what it built.
    pub async fn set_ready(
        &self,
        database_id: DatabaseId,
        db: ReadyDatabase<'_>,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            r#"
            UPDATE databases
            SET status = $2,
                container_id = $3,
                host = $4,
                port = $5,
                database_name = $6,
                owner_role = $7,
                owner_password_enc = $8,
                platform_password_enc = $9,
                error = NULL,
                updated_at = now()
            WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(DatabaseStatus::Ready.as_str())
        .bind(db.container_id)
        .bind(db.host)
        .bind(i32::from(db.port))
        .bind(db.database_name)
        .bind(db.owner_role)
        .bind(db.owner_password_enc)
        .bind(db.platform_password_enc)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Move a database to `deleting`. False when it's already deleting,
    /// deleted, or missing.
    pub async fn begin_delete(&self, database_id: DatabaseId) -> Result<bool, MetadataError> {
        let moved = sqlx::query(
            r#"
            UPDATE databases
            SET status = 'deleting', updated_at = now()
            WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
            "#,
        )
        .bind(database_id.as_uuid())
        .execute(&self.pool)
        .await?;
        Ok(moved.rows_affected() == 1)
    }

    /// Turn the row into a tombstone and forget everything that pointed at
    /// the (now removed) container and credentials.
    pub async fn mark_deleted(&self, database_id: DatabaseId) -> Result<(), MetadataError> {
        sqlx::query(
            r#"
            UPDATE databases
            SET status = 'deleted', deleted_at = now(), updated_at = now(),
                container_id = NULL, host = NULL, port = NULL,
                owner_password_enc = NULL, platform_password_enc = NULL
            WHERE id = $1
            "#,
        )
        .bind(database_id.as_uuid())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Store a new owner password for a ready database. False (nothing
    /// written) otherwise, e.g. when it was deleted while the reset ran.
    pub async fn set_owner_password_enc(
        &self,
        database_id: DatabaseId,
        owner_password_enc: &str,
    ) -> Result<bool, MetadataError> {
        let stored = sqlx::query(
            "UPDATE databases SET owner_password_enc = $2, updated_at = now()
             WHERE id = $1 AND status = 'ready'",
        )
        .bind(database_id.as_uuid())
        .bind(owner_password_enc)
        .execute(&self.pool)
        .await?;
        Ok(stored.rows_affected() == 1)
    }

    /// Fail a database stuck provisioning, but only if it is *still* pending or
    /// provisioning and untouched for `max_age_secs` at write time. Reconcile
    /// acts on a snapshot; this re-check keeps it from failing a database the
    /// provision consumer just picked up. True when it was failed.
    pub async fn fail_if_stale(
        &self,
        database_id: DatabaseId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        let failed = sqlx::query(
            r#"
            UPDATE databases
            SET status = 'failed', error = 'provisioning timed out', updated_at = now()
            WHERE id = $1
              AND status IN ('pending', 'provisioning')
              AND updated_at < now() - make_interval(secs => $2)
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(max_age_secs as f64)
        .execute(&self.pool)
        .await?;
        Ok(failed.rows_affected() == 1)
    }

    /// Compare the current secrets key with the one existing credentials were
    /// sealed with. `candidate` is a fresh check value for the current key and
    /// `opens` tells whether a stored check value opens with it.
    pub async fn ensure_key_check(
        &self,
        candidate: &str,
        opens: impl Fn(&str) -> bool,
    ) -> Result<KeyCheck, MetadataError> {
        const KEY: &str = "secrets_key_check";
        let inserted = sqlx::query(
            "INSERT INTO control_settings (key, value) VALUES ($1, $2) ON CONFLICT (key) DO NOTHING",
        )
        .bind(KEY)
        .bind(candidate)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 1 {
            return Ok(KeyCheck::Recorded);
        }
        let stored: String = sqlx::query_scalar("SELECT value FROM control_settings WHERE key = $1")
            .bind(KEY)
            .fetch_one(&self.pool)
            .await?;
        if opens(&stored) {
            return Ok(KeyCheck::Matches);
        }
        let sealed_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM databases
             WHERE owner_password_enc IS NOT NULL OR platform_password_enc IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        if sealed_rows > 0 {
            return Ok(KeyCheck::Mismatch { sealed_rows });
        }
        sqlx::query("UPDATE control_settings SET value = $2, updated_at = now() WHERE key = $1")
            .bind(KEY)
            .bind(candidate)
            .execute(&self.pool)
            .await?;
        Ok(KeyCheck::Replaced)
    }

    /// Which of `ids` belong to `deleted` tombstones (ids with no row at all
    /// are not returned).
    pub async fn deleted_among(
        &self,
        ids: &[DatabaseId],
    ) -> Result<std::collections::HashSet<DatabaseId>, MetadataError> {
        let uuids: Vec<Uuid> = ids.iter().map(|id| id.as_uuid()).collect();
        let rows = sqlx::query("SELECT id FROM databases WHERE status = 'deleted' AND id = ANY($1)")
            .bind(&uuids)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| DatabaseId(r.get("id"))).collect())
    }

    /// Every database that isn't deleted, with the age of its last change.
    pub async fn list_live_for_reconcile(&self) -> Result<Vec<LiveDatabase>, MetadataError> {
        let rows = sqlx::query(
            r#"
            SELECT id, status, EXTRACT(EPOCH FROM now() - updated_at)::BIGINT AS age_secs
            FROM databases
            WHERE status <> 'deleted'
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(LiveDatabase {
                    id: DatabaseId(r.get("id")),
                    status: DatabaseStatus::parse(r.get("status"))?,
                    age_secs: r.get("age_secs"),
                })
            })
            .collect()
    }

    /// Record a failure (ignored once a delete has begun).
    pub async fn set_failed(
        &self,
        database_id: DatabaseId,
        error: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            r#"
            UPDATE databases
            SET status = $2, error = $3, updated_at = now()
            WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(DatabaseStatus::Failed.as_str())
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<TenantId>, MetadataError> {
        let row: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM tenants WHERE zitadel_org_id = $1")
                .bind(zitadel_org_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(id,)| TenantId(id)))
    }

    pub async fn set_tenant_zitadel_org_id(
        &self,
        tenant_id: TenantId,
        zitadel_org_id: &str,
    ) -> Result<(), MetadataError> {
        let result = sqlx::query(
            r#"
            UPDATE tenants
            SET zitadel_org_id = $2
            WHERE id = $1
            "#,
        )
        .bind(tenant_id.as_uuid())
        .bind(zitadel_org_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(MetadataError::TenantNotFound(tenant_id));
        }
        Ok(())
    }

    pub async fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        let row = sqlx::query(
            r#"
            SELECT
                d.id,
                d.project_id,
                p.tenant_id,
                d.name,
                d.status,
                d.container_id,
                d.host,
                d.port,
                d.database_name,
                d.owner_role,
                d.owner_password_enc,
                d.platform_password_enc,
                d.error
            FROM databases d
            JOIN projects p ON p.id = d.project_id
            WHERE d.id = $1
            "#,
        )
        .bind(database_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_ref().map(database_from_row))
    }

    /// The tenant, its projects (by slug) and their databases (oldest first).
    pub async fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<TenantOverview>, MetadataError> {
        let Some(tenant) = sqlx::query("SELECT slug FROM tenants WHERE id = $1")
            .bind(tenant_id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let projects = sqlx::query(
            "SELECT id, slug FROM projects WHERE tenant_id = $1 ORDER BY slug",
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|r| (ProjectId(r.get("id")), r.get::<String, _>("slug")))
        .collect();
        let databases = sqlx::query(
            r#"
            SELECT d.id, d.project_id, p.tenant_id, d.name, d.status, d.container_id,
                   d.host, d.port, d.database_name, d.owner_role, d.owner_password_enc,
                   d.platform_password_enc, d.error
            FROM databases d
            JOIN projects p ON p.id = d.project_id
            WHERE p.tenant_id = $1 AND d.status <> 'deleted'
            ORDER BY d.created_at
            "#,
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(database_from_row)
        .collect();
        Ok(Some(group_overview(
            tenant_id,
            tenant.get("slug"),
            projects,
            databases,
        )))
    }
}

/// Map a `databases ⋈ projects` row (see `get_database`) to a record.
fn database_from_row(r: &sqlx::postgres::PgRow) -> DatabaseRecord {
    let port: Option<i32> = r.try_get("port").ok().flatten();
    DatabaseRecord {
        database_id: DatabaseId(r.get("id")),
        project_id: ProjectId(r.get("project_id")),
        tenant_id: TenantId(r.get("tenant_id")),
        name: r.get("name"),
        status: DatabaseStatus::parse(r.get("status")).unwrap_or(DatabaseStatus::Failed),
        container_id: r.get("container_id"),
        host: r.get("host"),
        port: port.and_then(|p| u16::try_from(p).ok()),
        database_name: r.get("database_name"),
        owner_role: r.get("owner_role"),
        owner_password_enc: r.get("owner_password_enc"),
        platform_password_enc: r.get("platform_password_enc"),
        error: r.get("error"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(project_id: ProjectId, name: &str) -> DatabaseRecord {
        DatabaseRecord {
            database_id: DatabaseId::new(),
            project_id,
            tenant_id: TenantId(DEMO_TENANT_ID),
            name: name.into(),
            status: DatabaseStatus::Ready,
            container_id: None,
            host: None,
            port: None,
            database_name: None,
            owner_role: None,
            owner_password_enc: None,
            platform_password_enc: None,
            error: None,
        }
    }

    #[test]
    fn database_names_are_sanitized_like_before() {
        assert_eq!(postgres_database_name("web-1 prod").unwrap(), "web_1_prod");
        assert_eq!(postgres_database_name("").unwrap(), "cheladb");
    }

    #[test]
    fn reserved_database_names_are_rejected() {
        for reserved in ["postgres", "template0", "template1", "POSTGRES", "Template1"] {
            assert!(matches!(
                postgres_database_name(reserved),
                Err(MetadataError::InvalidDatabaseName(_))
            ), "{reserved}");
        }
    }

    #[test]
    fn database_names_longer_than_63_bytes_are_rejected() {
        assert!(postgres_database_name(&"a".repeat(63)).is_ok());
        assert!(matches!(
            postgres_database_name(&"a".repeat(64)),
            Err(MetadataError::InvalidDatabaseName(_))
        ));
    }

    #[test]
    fn deleted_status_round_trips() {
        assert_eq!(DatabaseStatus::Deleted.as_str(), "deleted");
        assert_eq!(DatabaseStatus::parse("deleted").unwrap(), DatabaseStatus::Deleted);
    }

    #[test]
    fn group_overview_attaches_databases_to_their_projects() {
        let (a, b) = (ProjectId::new(), ProjectId::new());
        let overview = group_overview(
            TenantId(DEMO_TENANT_ID),
            "demo".into(),
            vec![(a, "alpha".into()), (b, "beta".into())],
            vec![record(b, "b1"), record(a, "a1"), record(b, "b2")],
        );
        assert_eq!(overview.slug, "demo");
        let names = |i: usize| -> Vec<String> {
            overview.projects[i]
                .databases
                .iter()
                .map(|d| d.name.clone())
                .collect()
        };
        assert_eq!(overview.projects[0].slug, "alpha");
        assert_eq!(names(0), vec!["a1"]);
        assert_eq!(names(1), vec!["b1", "b2"]);
    }

    #[test]
    fn group_overview_keeps_projects_without_databases() {
        let a = ProjectId::new();
        let overview = group_overview(
            TenantId(DEMO_TENANT_ID),
            "demo".into(),
            vec![(a, "alpha".into())],
            vec![],
        );
        assert_eq!(overview.projects.len(), 1);
        assert!(overview.projects[0].databases.is_empty());
    }

    #[test]
    fn group_overview_drops_databases_of_unlisted_projects() {
        let a = ProjectId::new();
        let overview = group_overview(
            TenantId(DEMO_TENANT_ID),
            "demo".into(),
            vec![(a, "alpha".into())],
            vec![record(ProjectId::new(), "stray")],
        );
        assert!(overview.projects[0].databases.is_empty());
    }
}
