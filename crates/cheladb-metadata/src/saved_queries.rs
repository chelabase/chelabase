//! SQL editor saved queries: private to one user, per Cheladb database.

use chrono::{DateTime, Utc};
use sqlx::postgres::PgRow;
use sqlx::{QueryBuilder, Row};
use uuid::Uuid;

use crate::{MetadataClient, MetadataError};

/// Saved queries one user may keep per database.
pub const SAVED_QUERY_CAP: i64 = 500;

const NAME_INDEX: &str = "saved_queries_name";
macro_rules! columns {
    () => {
        "id, name, sql, pg_database, role, version, created_at, updated_at"
    };
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SavedQuery {
    pub id: Uuid,
    pub name: String,
    pub sql: String,
    pub pg_database: Option<String>,
    pub role: Option<String>,
    pub version: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SavedQuerySummary {
    pub id: Uuid,
    pub name: String,
    pub pg_database: Option<String>,
    pub role: Option<String>,
    pub version: i32,
    pub updated_at: DateTime<Utc>,
}

/// Fields to change; `None` leaves a field, `Some(None)` clears a hint.
#[derive(Debug, Clone, Default)]
pub struct SavedQueryPatch {
    pub name: Option<String>,
    pub sql: Option<String>,
    pub pg_database: Option<Option<String>>,
    pub role: Option<Option<String>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SavedQueryWrite {
    Saved(SavedQuery),
    NameTaken,
    CapReached,
    /// The version didn't match; the current row.
    Stale(SavedQuery),
    NotFound,
}

fn saved_from_row(r: &PgRow) -> SavedQuery {
    SavedQuery {
        id: r.get("id"),
        name: r.get("name"),
        sql: r.get("sql"),
        pg_database: r.get("pg_database"),
        role: r.get("role"),
        version: r.get("version"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

fn is_name_taken(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(e) if e.constraint() == Some(NAME_INDEX))
}

impl MetadataClient {
    /// The user's saved queries for a database, by lower-cased name.
    pub async fn list_saved_queries(
        &self,
        database: Uuid,
        user: &str,
    ) -> Result<Vec<SavedQuerySummary>, MetadataError> {
        let rows = sqlx::query(
            "SELECT id, name, pg_database, role, version, updated_at FROM saved_queries
             WHERE database_id = $1 AND user_id = $2 ORDER BY lower(name), id",
        )
        .bind(database)
        .bind(user)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| SavedQuerySummary {
                id: r.get("id"),
                name: r.get("name"),
                pg_database: r.get("pg_database"),
                role: r.get("role"),
                version: r.get("version"),
                updated_at: r.get("updated_at"),
            })
            .collect())
    }

    pub async fn get_saved_query(
        &self,
        database: Uuid,
        user: &str,
        id: Uuid,
    ) -> Result<Option<SavedQuery>, MetadataError> {
        Ok(sqlx::query(concat!(
            "SELECT ",
            columns!(),
            " FROM saved_queries WHERE id = $1 AND database_id = $2 AND user_id = $3"
        ))
        .bind(id)
        .bind(database)
        .bind(user)
        .fetch_optional(&self.pool)
        .await?
        .as_ref()
        .map(saved_from_row))
    }

    /// `Saved`, `NameTaken` (same name, any case) or `CapReached` ([`SAVED_QUERY_CAP`]). One
    /// advisory lock per (database, user) keeps concurrent creates from passing the cap.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_saved_query(
        &self,
        database: Uuid,
        user: &str,
        id: Uuid,
        name: &str,
        sql: &str,
        pg_database: Option<&str>,
        role: Option<&str>,
    ) -> Result<SavedQueryWrite, MetadataError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || '/' || $2, 0))")
            .bind(database)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM saved_queries WHERE database_id = $1 AND user_id = $2",
        )
        .bind(database)
        .bind(user)
        .fetch_one(&mut *tx)
        .await?;
        if count >= SAVED_QUERY_CAP {
            return Ok(SavedQueryWrite::CapReached);
        }
        let inserted = sqlx::query(concat!(
            "INSERT INTO saved_queries (id, database_id, user_id, name, sql, pg_database, role)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING ",
            columns!()
        ))
        .bind(id)
        .bind(database)
        .bind(user)
        .bind(name)
        .bind(sql)
        .bind(pg_database)
        .bind(role)
        .fetch_one(&mut *tx)
        .await;
        match inserted {
            Ok(row) => {
                tx.commit().await?;
                Ok(SavedQueryWrite::Saved(saved_from_row(&row)))
            }
            Err(err) if is_name_taken(&err) => Ok(SavedQueryWrite::NameTaken),
            Err(err) => Err(err.into()),
        }
    }

    /// Applies `patch` when `version` is current: `Saved`, `NameTaken`, `Stale` (with the
    /// current row) or `NotFound`.
    pub async fn update_saved_query(
        &self,
        database: Uuid,
        user: &str,
        id: Uuid,
        version: i32,
        patch: &SavedQueryPatch,
    ) -> Result<SavedQueryWrite, MetadataError> {
        let mut q = QueryBuilder::new("UPDATE saved_queries SET version = version + 1, updated_at = now()");
        if let Some(name) = &patch.name {
            q.push(", name = ").push_bind(name);
        }
        if let Some(sql) = &patch.sql {
            q.push(", sql = ").push_bind(sql);
        }
        if let Some(pg_database) = &patch.pg_database {
            q.push(", pg_database = ").push_bind(pg_database);
        }
        if let Some(role) = &patch.role {
            q.push(", role = ").push_bind(role);
        }
        q.push(" WHERE id = ").push_bind(id);
        q.push(" AND database_id = ").push_bind(database);
        q.push(" AND user_id = ").push_bind(user);
        q.push(" AND version = ").push_bind(version);
        q.push(concat!(" RETURNING ", columns!()));
        match q.build().fetch_optional(&self.pool).await {
            Ok(Some(row)) => Ok(SavedQueryWrite::Saved(saved_from_row(&row))),
            Ok(None) => Ok(match self.get_saved_query(database, user, id).await? {
                Some(current) => SavedQueryWrite::Stale(current),
                None => SavedQueryWrite::NotFound,
            }),
            Err(err) if is_name_taken(&err) => Ok(SavedQueryWrite::NameTaken),
            Err(err) => Err(err.into()),
        }
    }

    /// `false` when there was nothing to delete.
    pub async fn delete_saved_query(
        &self,
        database: Uuid,
        user: &str,
        id: Uuid,
    ) -> Result<bool, MetadataError> {
        Ok(sqlx::query(
            "DELETE FROM saved_queries WHERE id = $1 AND database_id = $2 AND user_id = $3",
        )
        .bind(id)
        .bind(database)
        .bind(user)
        .execute(&self.pool)
        .await?
        .rows_affected()
            > 0)
    }
}
