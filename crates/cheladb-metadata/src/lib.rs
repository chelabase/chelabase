//! Metadata store: Tenant → Project → Database lookup and provisioning state.

use cheladb_types::{DatabaseId, ProjectId, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

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
    /// Dev-only credential for local `psql` poking.
    pub admin_password: Option<String>,
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
}

impl DatabaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Deleting => "deleting",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "pending" => Ok(Self::Pending),
            "provisioning" => Ok(Self::Provisioning),
            "ready" => Ok(Self::Ready),
            "failed" => Ok(Self::Failed),
            "deleting" => Ok(Self::Deleting),
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
                if err.constraint() == Some("databases_project_id_name_key") =>
            {
                Err(MetadataError::DuplicateDatabaseName)
            }
            Err(err) => Err(err.into()),
        }
    }

    pub async fn set_status(
        &self,
        database_id: DatabaseId,
        status: DatabaseStatus,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            r#"
            UPDATE databases
            SET status = $2, error = NULL, updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(status.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_ready(
        &self,
        database_id: DatabaseId,
        container_id: &str,
        host: &str,
        port: u16,
        admin_password: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            r#"
            UPDATE databases
            SET status = $2,
                container_id = $3,
                host = $4,
                port = $5,
                admin_password = $6,
                error = NULL,
                updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(DatabaseStatus::Ready.as_str())
        .bind(container_id)
        .bind(host)
        .bind(i32::from(port))
        .bind(admin_password)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn set_failed(
        &self,
        database_id: DatabaseId,
        error: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            r#"
            UPDATE databases
            SET status = $2, error = $3, updated_at = now()
            WHERE id = $1
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
                d.admin_password,
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
                   d.host, d.port, d.admin_password, d.error
            FROM databases d
            JOIN projects p ON p.id = d.project_id
            WHERE p.tenant_id = $1
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
        admin_password: r.get("admin_password"),
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
            admin_password: None,
            error: None,
        }
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
