//! Data API metadata: keys, the authenticator password and the per-database settings the REST
//! service reads.
//!
//! Lock order: see `branches.rs`. Only [`MetadataClient::rotate_data_key`] spans more than one
//! statement; it locks the database row, then the branch row, then touches key rows. The other
//! writes are single statements on one table.

use std::fmt;

use cheladb_types::BranchId;
use cheladb_types::data_key::DataKeyKind;
use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::branches::lock_database_of_branch;
use crate::{BranchStatus, MetadataClient, MetadataError};

/// The unique index allowing one live public key per branch.
const ONE_LIVE_PUBLIC: &str = "data_api_keys_one_live_public";

/// A key to store. `sealed` is the key sealed for display (public keys only).
#[derive(Clone)]
pub struct NewDataKey {
    pub id: Uuid,
    pub branch_id: BranchId,
    pub kind: DataKeyKind,
    pub role: String,
    pub name: String,
    pub prefix: String,
    pub sha256: [u8; 32],
    pub sealed: Option<String>,
}

impl fmt::Debug for NewDataKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewDataKey")
            .field("id", &self.id)
            .field("branch_id", &self.branch_id)
            .field("kind", &self.kind)
            .field("role", &self.role)
            .field("name", &self.name)
            .field("prefix", &self.prefix)
            .field("sha256", &"<redacted>")
            .field("sealed", &self.sealed.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct DataKeyRecord {
    pub id: Uuid,
    pub branch_id: BranchId,
    pub kind: DataKeyKind,
    pub role: String,
    pub name: String,
    pub prefix: String,
    pub sealed: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
}

impl fmt::Debug for DataKeyRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DataKeyRecord")
            .field("id", &self.id)
            .field("branch_id", &self.branch_id)
            .field("kind", &self.kind)
            .field("role", &self.role)
            .field("name", &self.name)
            .field("prefix", &self.prefix)
            .field("sealed", &self.sealed.as_ref().map(|_| "<redacted>"))
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("revoked_at", &self.revoked_at)
            .finish()
    }
}

/// What the gateway learns from a key's hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataKeyLookup {
    pub key_id: Uuid,
    pub branch_id: BranchId,
    pub kind: DataKeyKind,
    pub role: String,
}

/// A database's data API settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataApiSettings {
    pub max_rows: i32,
    pub statement_timeout_ms: i32,
    pub max_connections: i32,
    pub default_schema: String,
}

/// What the REST service needs to connect to a branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchConnectionRow {
    pub branch_id: BranchId,
    pub status: BranchStatus,
    pub endpoint_id: Option<String>,
    /// The Postgres database inside the compute (the database's, shared by its branches).
    pub database_name: Option<String>,
    pub authenticator_password_enc: Option<String>,
    pub settings: DataApiSettings,
}

const KEY_COLUMNS: &str = "id, branch_id, kind, role, name, prefix, sealed, created_at, \
     last_used_at, revoked_at";

fn parse_kind(value: &str) -> Result<DataKeyKind, MetadataError> {
    DataKeyKind::parse(value)
        .ok_or_else(|| MetadataError::InvalidStatus(format!("data key kind {value}")))
}

fn key_from_row(r: &PgRow) -> Result<DataKeyRecord, MetadataError> {
    Ok(DataKeyRecord {
        id: r.get("id"),
        branch_id: BranchId(r.get("branch_id")),
        kind: parse_kind(r.get("kind"))?,
        role: r.get("role"),
        name: r.get("name"),
        prefix: r.get("prefix"),
        sealed: r.get("sealed"),
        created_at: r.get("created_at"),
        last_used_at: r.get("last_used_at"),
        revoked_at: r.get("revoked_at"),
    })
}

const INSERT_KEY: &str =
    "INSERT INTO data_api_keys (id, branch_id, kind, role, name, prefix, sha256, sealed)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)";

/// A second live public key on a branch is a [`MetadataError::Conflict`]; anything else passes.
fn map_insert_error(err: sqlx::Error) -> MetadataError {
    match err {
        sqlx::Error::Database(e) if e.constraint() == Some(ONE_LIVE_PUBLIC) => {
            MetadataError::Conflict
        }
        err => err.into(),
    }
}

impl MetadataClient {
    /// Stores a key. A second live public key on the branch is [`MetadataError::Conflict`].
    pub async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        bind_new_key(sqlx::query(INSERT_KEY), key, key.branch_id)
            .execute(&self.pool)
            .await
            .map_err(map_insert_error)?;
        Ok(())
    }

    /// The branch's live keys, newest first.
    pub async fn list_data_keys(
        &self,
        branch: BranchId,
    ) -> Result<Vec<DataKeyRecord>, MetadataError> {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT {KEY_COLUMNS} FROM data_api_keys
             WHERE branch_id = $1 AND revoked_at IS NULL
             ORDER BY created_at DESC, id DESC"
        )))
        .bind(branch.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(key_from_row)
        .collect()
    }

    /// The live key with this hash, if its branch isn't deleting or deleted.
    pub async fn lookup_data_key(
        &self,
        sha256: &[u8; 32],
    ) -> Result<Option<DataKeyLookup>, MetadataError> {
        let row = sqlx::query(
            "SELECT k.id, k.branch_id, k.kind, k.role
             FROM data_api_keys k
             JOIN branches b ON b.id = k.branch_id
             WHERE k.sha256 = $1 AND k.revoked_at IS NULL
               AND b.status NOT IN ('deleting', 'deleted')",
        )
        .bind(sha256.as_slice())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            Ok(DataKeyLookup {
                key_id: r.get("id"),
                branch_id: BranchId(r.get("branch_id")),
                kind: parse_kind(r.get("kind"))?,
                role: r.get("role"),
            })
        })
        .transpose()
    }

    /// Stamps `last_used_at`, at most once a minute per key.
    pub async fn touch_data_key(&self, key_id: Uuid) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE data_api_keys SET last_used_at = now()
             WHERE id = $1 AND (last_used_at IS NULL OR last_used_at < now() - interval '60 seconds')",
        )
        .bind(key_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Revokes a live secret key of the branch. False for an unknown or already revoked key,
    /// and for a public key (rotate it instead: a branch always has one).
    pub async fn revoke_data_key(
        &self,
        branch: BranchId,
        key_id: Uuid,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE data_api_keys SET revoked_at = now()
             WHERE id = $1 AND branch_id = $2 AND kind = 'secret' AND revoked_at IS NULL",
        )
        .bind(key_id)
        .bind(branch.as_uuid())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Revokes the live key `old` of `branch` and stores `new` (for that branch) in one
    /// transaction. False, nothing written, when `old` isn't a live key of the branch. A failing
    /// insert (a second live public key: [`MetadataError::Conflict`]) rolls the revoke back.
    pub async fn rotate_data_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_database_of_branch(&mut tx, branch).await?;
        sqlx::query("SELECT 1 FROM branches WHERE id = $1 FOR NO KEY UPDATE")
            .bind(branch.as_uuid())
            .execute(&mut *tx)
            .await?;
        let revoked = sqlx::query(
            "UPDATE data_api_keys SET revoked_at = now()
             WHERE id = $1 AND branch_id = $2 AND revoked_at IS NULL",
        )
        .bind(old)
        .bind(branch.as_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if revoked != 1 {
            return Ok(false);
        }
        bind_new_key(sqlx::query(INSERT_KEY), new, branch)
            .execute(&mut *tx)
            .await
            .map_err(map_insert_error)?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn has_live_public_key(&self, branch: BranchId) -> Result<bool, MetadataError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM data_api_keys
                            WHERE branch_id = $1 AND kind = 'public' AND revoked_at IS NULL)",
        )
        .bind(branch.as_uuid())
        .fetch_one(&self.pool)
        .await?)
    }

    /// Records the sealed `authenticator` password of a live branch that has none. A branch
    /// that already has one keeps it: two runs racing to store one agree on the first, and
    /// each re-reads what was stored.
    pub async fn set_authenticator_password(
        &self,
        branch: BranchId,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE branches SET authenticator_password_enc = $2, updated_at = now()
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
               AND authenticator_password_enc IS NULL",
        )
        .bind(branch.as_uuid())
        .bind(sealed)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Everything the REST service needs to connect to `branch`, whatever its status.
    pub async fn branch_connection(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchConnectionRow>, MetadataError> {
        let row = sqlx::query(
            "SELECT b.id, b.status, b.endpoint_id, b.authenticator_password_enc,
                    d.database_name, d.max_rows, d.statement_timeout_ms, d.max_connections,
                    d.default_schema
             FROM branches b
             JOIN databases d ON d.id = b.database_id
             WHERE b.id = $1",
        )
        .bind(branch.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            Ok(BranchConnectionRow {
                branch_id: BranchId(r.get("id")),
                status: BranchStatus::parse(r.get("status"))?,
                endpoint_id: r.get("endpoint_id"),
                database_name: r.get("database_name"),
                authenticator_password_enc: r.get("authenticator_password_enc"),
                settings: DataApiSettings {
                    max_rows: r.get("max_rows"),
                    statement_timeout_ms: r.get("statement_timeout_ms"),
                    max_connections: r.get("max_connections"),
                    default_schema: r.get("default_schema"),
                },
            })
        })
        .transpose()
    }

    /// Ready branches (backups have no compute and are skipped) that still need the data API
    /// set up: no live public key, or no authenticator password. Oldest first.
    pub async fn branches_missing_data_api(&self) -> Result<Vec<BranchId>, MetadataError> {
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT b.id FROM branches b
             WHERE b.status = 'ready' AND b.kind = 'normal'
               AND (b.authenticator_password_enc IS NULL
                    OR NOT EXISTS (SELECT 1 FROM data_api_keys k
                                   WHERE k.branch_id = b.id AND k.kind = 'public'
                                     AND k.revoked_at IS NULL))
             ORDER BY b.created_at, b.id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(ids.into_iter().map(BranchId).collect())
    }
}

fn bind_new_key<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    key: &'q NewDataKey,
    branch: BranchId,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    query
        .bind(key.id)
        .bind(branch.as_uuid())
        .bind(key.kind.as_str())
        .bind(key.role.as_str())
        .bind(key.name.as_str())
        .bind(key.prefix.as_str())
        .bind(key.sha256.as_slice())
        .bind(key.sealed.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_the_hash_and_the_sealed_key() {
        let key = NewDataKey {
            id: Uuid::nil(),
            branch_id: BranchId::new(),
            kind: DataKeyKind::Public,
            role: "anon".into(),
            name: "default".into(),
            prefix: "chk_pub_abcd".into(),
            sha256: [0xab; 32],
            sealed: Some("v1:SECRETSEALED".into()),
        };
        let shown = format!("{key:?}");
        assert!(
            !shown.contains("SECRETSEALED") && !shown.contains("171"),
            "{shown}"
        );
        assert!(shown.contains("chk_pub_abcd"));
        let record = DataKeyRecord {
            id: key.id,
            branch_id: key.branch_id,
            kind: key.kind,
            role: key.role.clone(),
            name: key.name.clone(),
            prefix: key.prefix.clone(),
            sealed: key.sealed.clone(),
            created_at: Utc::now(),
            last_used_at: None,
            revoked_at: None,
        };
        assert!(!format!("{record:?}").contains("SECRETSEALED"));
    }
}
