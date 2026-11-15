//! s3-endpoint m1: S3 access keys, each bound to one branch, one Postgres database on it and
//! one role (by its OID on a timeline, never just its name).
//!
//! SigV4 needs the secret itself, so it is stored sealed (`Purpose::S3Key`, AAD the key's id):
//! [`MetadataClient::create_s3_key`] seals it with this client's secrets key, and only
//! [`MetadataClient::lookup_s3_key`] opens it. A value copied from another row never opens.
//! Revoking clears it in the same statement; the table's CHECKs refuse a revoked key that keeps
//! a secret, a live key without one, and a live key without a role identity.
//!
//! s3-endpoint m6: a key follows or dies with its role and database exactly where a secret
//! data key does (the DDL receiver, role delete and rename, a dropped or renamed database, the
//! resync's [`MetadataClient::fix_s3_key_roles`]). Each of those writes returns the ids of the
//! live S3 keys it changed, so the caller tells storage after the commit. Inside one
//! transaction the lock order is data keys first, then S3 keys, at every site.

use std::fmt;

use chelabase_types::{BranchId, DatabaseId, ProjectId};
use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::{BranchStatus, DatabaseStatus, KeyRoleFix, MetadataClient, MetadataError};

/// Live keys one branch may have.
pub const STORAGE_S3_KEY_CAP: i64 = 100;

/// A new key. `secret` is the plaintext: [`MetadataClient::create_s3_key`] seals it bound to
/// `id`, so a caller can't store a value sealed for another row.
#[derive(Clone)]
pub struct NewS3Key<'a> {
    pub id: Uuid,
    pub branch_id: BranchId,
    /// The Postgres database on the branch.
    pub database: &'a str,
    pub role: &'a str,
    pub name: &'a str,
    pub access_key_id: &'a str,
    pub secret: &'a str,
    /// The role's `pg_roles.oid`, read on `role_timeline`.
    pub role_oid: i64,
    pub role_timeline: &'a str,
}

impl fmt::Debug for NewS3Key<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewS3Key")
            .field("id", &self.id)
            .field("branch_id", &self.branch_id)
            .field("database", &self.database)
            .field("role", &self.role)
            .field("name", &self.name)
            .field("access_key_id", &self.access_key_id)
            .field("role_oid", &self.role_oid)
            .field("role_timeline", &self.role_timeline)
            .finish_non_exhaustive()
    }
}

/// A live key as listed (never its secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3KeyRecord {
    pub id: Uuid,
    pub branch_id: BranchId,
    pub database: String,
    pub role: String,
    pub name: String,
    pub access_key_id: String,
    pub role_oid: i64,
    pub role_timeline: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// A live key found by its access key id, with its opened secret.
#[derive(Clone, PartialEq, Eq)]
pub struct S3KeyLookup {
    pub key_id: Uuid,
    pub branch_id: BranchId,
    pub branch_status: BranchStatus,
    /// The Chelabase database the branch belongs to, and its status.
    pub database_id: DatabaseId,
    pub database_status: DatabaseStatus,
    pub project_id: ProjectId,
    /// The Postgres database on the branch the key reaches.
    pub database: String,
    pub role: String,
    /// The role's identity. The table's CHECK keeps both set on a live key; they are read as
    /// they are stored all the same, so a caller refuses a key without them instead of this
    /// lookup panicking on a NULL.
    pub role_oid: Option<i64>,
    pub role_timeline: Option<String>,
    pub secret: String,
}

impl fmt::Debug for S3KeyLookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3KeyLookup")
            .field("key_id", &self.key_id)
            .field("branch_id", &self.branch_id)
            .field("branch_status", &self.branch_status)
            .field("database_id", &self.database_id)
            .field("database_status", &self.database_status)
            .field("project_id", &self.project_id)
            .field("database", &self.database)
            .field("role", &self.role)
            .field("role_oid", &self.role_oid)
            .field("role_timeline", &self.role_timeline)
            .field("secret", &"<redacted>")
            .finish()
    }
}

fn record_from_row(r: &PgRow) -> S3KeyRecord {
    S3KeyRecord {
        id: r.get("id"),
        branch_id: BranchId(r.get("branch_id")),
        database: r.get("database"),
        role: r.get("role"),
        name: r.get("name"),
        access_key_id: r.get("access_key_id"),
        role_oid: r.get("role_oid"),
        role_timeline: r.get("role_timeline"),
        created_at: r.get("created_at"),
        last_used_at: r.get("last_used_at"),
    }
}

/// The branch's live keys bound to `role`: `(id, name)`, oldest first.
pub(crate) async fn live_s3_keys_of_role(
    conn: &mut sqlx::PgConnection,
    branch: BranchId,
    role: &str,
) -> Result<Vec<(Uuid, String)>, MetadataError> {
    Ok(sqlx::query_as(
        "SELECT id, name FROM storage_s3_keys
         WHERE branch_id = $1 AND role = $2 AND revoked_at IS NULL
         ORDER BY created_at, id",
    )
    .bind(branch.as_uuid())
    .bind(role)
    .fetch_all(conn)
    .await?)
}

/// A role renamed `old` -> `new`: the branch's keys of `old` follow, live and revoked alike
/// (as data keys do). Returns the live ones' ids.
pub(crate) async fn follow_role_rename(
    conn: &mut sqlx::PgConnection,
    branch: BranchId,
    old: &str,
    new: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "WITH moved AS (
             UPDATE storage_s3_keys SET role = $3 WHERE branch_id = $1 AND role = $2
             RETURNING id, revoked_at
         )
         SELECT id FROM moved WHERE revoked_at IS NULL ORDER BY id",
    )
    .bind(branch.as_uuid())
    .bind(old)
    .bind(new)
    .fetch_all(conn)
    .await
}

/// A database renamed `old` -> `new` on the branch: its keys follow, live and revoked alike.
/// Returns the live ones' ids.
pub(crate) async fn follow_database_rename(
    conn: &mut sqlx::PgConnection,
    branch: BranchId,
    old: &str,
    new: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "WITH moved AS (
             UPDATE storage_s3_keys SET database = $3 WHERE branch_id = $1 AND database = $2
             RETURNING id, revoked_at
         )
         SELECT id FROM moved WHERE revoked_at IS NULL ORDER BY id",
    )
    .bind(branch.as_uuid())
    .bind(old)
    .bind(new)
    .fetch_all(conn)
    .await
}

/// A database dropped on the branch: its live keys are revoked, secrets cleared. Returns their
/// ids.
pub(crate) async fn revoke_for_dropped_database(
    conn: &mut sqlx::PgConnection,
    branch: BranchId,
    name: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "WITH revoked AS (
             UPDATE storage_s3_keys SET revoked_at = now(), secret_enc = NULL
             WHERE branch_id = $1 AND database = $2 AND revoked_at IS NULL
             RETURNING id
         )
         SELECT id FROM revoked ORDER BY id",
    )
    .bind(branch.as_uuid())
    .bind(name)
    .fetch_all(conn)
    .await
}

/// A role dropped through the API: the live keys bound to `role` on the branch linked to
/// `endpoint_id` are revoked, secrets cleared. Returns their ids.
pub(crate) async fn revoke_role_on_endpoint(
    conn: &mut sqlx::PgConnection,
    endpoint_id: &str,
    role: &str,
) -> Result<Vec<Uuid>, sqlx::Error> {
    sqlx::query_scalar(
        "WITH revoked AS (
             UPDATE storage_s3_keys k SET revoked_at = now(), secret_enc = NULL
             FROM branches b
             WHERE b.endpoint_id = $1 AND k.branch_id = b.id
               AND k.role = $2 AND k.revoked_at IS NULL
             RETURNING k.id
         )
         SELECT id FROM revoked ORDER BY id",
    )
    .bind(endpoint_id)
    .bind(role)
    .fetch_all(conn)
    .await
}

impl MetadataClient {
    fn s3_secrets_key(&self) -> Result<&chelabase_secrets::SecretKey, MetadataError> {
        self.secrets_key
            .as_deref()
            .ok_or_else(|| MetadataError::Config("this metadata client has no secrets key".into()))
    }

    /// Stores a key, its secret sealed for its id. A branch with [`STORAGE_S3_KEY_CAP`] live
    /// keys already is [`MetadataError::Conflict`] (counted under the branch row's lock, so two
    /// creates can't both pass it); a missing, deleting or deleted branch, or one that isn't
    /// `normal` (a backup or rehearsal, which lookup never serves), is `RowNotFound`. Nothing is
    /// written either way.
    ///
    /// Lock order: the branch row (`FOR NO KEY UPDATE`), then its endpoint row (`FOR SHARE`),
    /// as secret data key creation takes them. The DDL receiver holds the endpoint row, so a
    /// `DROP ROLE` forwarded after the store sees the key.
    pub async fn create_s3_key(&self, key: &NewS3Key<'_>) -> Result<(), MetadataError> {
        let sealed = chelabase_secrets::seal_s3_secret(self.s3_secrets_key()?, key.id, key.secret);
        let mut tx = self.pool.begin().await?;
        crate::data_api::lock_endpoint_of_branch(&mut tx, key.branch_id).await?;
        let usable: bool = sqlx::query_scalar(
            "SELECT kind = 'normal' AND status NOT IN ('deleting', 'deleted')
             FROM branches WHERE id = $1",
        )
        .bind(key.branch_id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if !usable {
            return Err(sqlx::Error::RowNotFound.into());
        }
        let live: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM storage_s3_keys WHERE branch_id = $1 AND revoked_at IS NULL",
        )
        .bind(key.branch_id.as_uuid())
        .fetch_one(&mut *tx)
        .await?;
        if live >= STORAGE_S3_KEY_CAP {
            return Err(MetadataError::Conflict);
        }
        sqlx::query(
            "INSERT INTO storage_s3_keys
                 (id, branch_id, database, role, name, access_key_id, secret_enc, role_oid, role_timeline)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(key.id)
        .bind(key.branch_id.as_uuid())
        .bind(key.database)
        .bind(key.role)
        .bind(key.name)
        .bind(key.access_key_id)
        .bind(&sealed)
        .bind(key.role_oid)
        .bind(key.role_timeline)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// The branch's live keys, newest first.
    pub async fn list_live_s3_keys(
        &self,
        branch: BranchId,
    ) -> Result<Vec<S3KeyRecord>, MetadataError> {
        let rows = sqlx::query(
            "SELECT id, branch_id, database, role, name, access_key_id, role_oid, role_timeline,
                    created_at, last_used_at
             FROM storage_s3_keys WHERE branch_id = $1 AND revoked_at IS NULL
             ORDER BY created_at DESC, id DESC",
        )
        .bind(branch.as_uuid())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(record_from_row).collect())
    }

    /// Applies the resync's [`KeyRoleFix`]es to the branch's live S3 keys in one transaction,
    /// each only while the row still has the role, OID and timeline the plan saw (a key
    /// renamed, re-stamped or revoked meanwhile is left alone). An S3 key always has an OID,
    /// so a `Backfill` is never planned for one; if one comes, the key is revoked. `Bypass`
    /// doesn't apply to S3 keys and is skipped. Returns the ids of the keys changed.
    pub async fn fix_s3_key_roles(
        &self,
        branch: BranchId,
        fixes: &[KeyRoleFix],
    ) -> Result<Vec<Uuid>, MetadataError> {
        if fixes.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.pool.begin().await?;
        let mut changed = Vec::new();
        for fix in fixes {
            let query = match fix {
                KeyRoleFix::Revoke { id, role, oid } => sqlx::query_scalar(
                    "UPDATE storage_s3_keys SET revoked_at = now(), secret_enc = NULL
                     WHERE id = $1 AND branch_id = $2 AND revoked_at IS NULL
                       AND role = $3 AND role_oid IS NOT DISTINCT FROM $4
                     RETURNING id",
                )
                .bind(*id)
                .bind(branch.as_uuid())
                .bind(role)
                .bind(*oid),
                KeyRoleFix::Follow { id, from, to, oid, timeline } => sqlx::query_scalar(
                    "UPDATE storage_s3_keys SET role = $4
                     WHERE id = $1 AND branch_id = $2 AND revoked_at IS NULL
                       AND role = $3 AND role_oid = $5 AND role_timeline = $6
                     RETURNING id",
                )
                .bind(*id)
                .bind(branch.as_uuid())
                .bind(from)
                .bind(to)
                .bind(*oid)
                .bind(timeline),
                KeyRoleFix::Restamp { id, role, oid, from, timeline } => sqlx::query_scalar(
                    "UPDATE storage_s3_keys SET role_timeline = $5
                     WHERE id = $1 AND branch_id = $2 AND revoked_at IS NULL
                       AND role = $3 AND role_oid = $4
                       AND role_timeline IS NOT DISTINCT FROM $6
                     RETURNING id",
                )
                .bind(*id)
                .bind(branch.as_uuid())
                .bind(role)
                .bind(*oid)
                .bind(timeline)
                .bind(from.as_deref()),
                KeyRoleFix::Backfill { id, role, .. } => sqlx::query_scalar(
                    "UPDATE storage_s3_keys SET revoked_at = now(), secret_enc = NULL
                     WHERE id = $1 AND branch_id = $2 AND revoked_at IS NULL AND role = $3
                     RETURNING id",
                )
                .bind(*id)
                .bind(branch.as_uuid())
                .bind(role),
                KeyRoleFix::Bypass { .. } => continue,
            };
            let id: Option<Uuid> = query.fetch_optional(&mut *tx).await?;
            changed.extend(id);
        }
        tx.commit().await?;
        Ok(changed)
    }

    /// Revokes a live key of the branch: `revoked_at` set and the sealed secret erased in one
    /// statement. False for an unknown, another branch's or an already revoked key.
    pub async fn revoke_s3_key(&self, branch: BranchId, id: Uuid) -> Result<bool, MetadataError> {
        let done = sqlx::query(
            "UPDATE storage_s3_keys SET revoked_at = now(), secret_enc = NULL
             WHERE id = $1 AND branch_id = $2 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(branch.as_uuid())
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() == 1)
    }

    /// The live key with this access key id, with its opened secret, if its branch is a
    /// `normal` one and neither the branch nor its database is deleting or deleted. A sealed
    /// value that doesn't open for this row (copied from another key, or another secrets key)
    /// is [`MetadataError::Secret`]: never a secret.
    pub async fn lookup_s3_key(
        &self,
        access_key_id: &str,
    ) -> Result<Option<S3KeyLookup>, MetadataError> {
        let key = self.s3_secrets_key()?;
        let Some(r) = sqlx::query(
            "SELECT k.id, k.branch_id, b.status AS branch_status, d.id AS database_id,
                    d.status AS database_status, d.project_id, k.database, k.role, k.role_oid,
                    k.role_timeline, k.secret_enc
             FROM storage_s3_keys k
             JOIN branches b ON b.id = k.branch_id
             JOIN databases d ON d.id = b.database_id
             WHERE k.access_key_id = $1 AND k.revoked_at IS NULL
               AND b.kind = 'normal' AND b.status NOT IN ('deleting', 'deleted')
               AND d.status NOT IN ('deleting', 'deleted')",
        )
        .bind(access_key_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let id: Uuid = r.get("id");
        let sealed: String = r.get("secret_enc");
        let secret = chelabase_secrets::open_s3_secret(key, id, &sealed)
            .map_err(|e| MetadataError::Secret(format!("S3 key {id}: {e}")))?;
        Ok(Some(S3KeyLookup {
            key_id: id,
            branch_id: BranchId(r.get("branch_id")),
            branch_status: BranchStatus::parse(r.get("branch_status"))?,
            database_id: DatabaseId(r.get("database_id")),
            database_status: DatabaseStatus::parse(r.get("database_status"))?,
            project_id: ProjectId(r.get("project_id")),
            database: r.get("database"),
            role: r.get("role"),
            role_oid: r.get::<Option<i64>, _>("role_oid"),
            role_timeline: r.get::<Option<String>, _>("role_timeline"),
            secret,
        }))
    }

    /// Stamps a live key's `last_used_at`, at most once a minute. Never fails: an error is
    /// logged, so a lookup never depends on it (callers may run it in the background).
    pub async fn touch_s3_key_used(&self, id: Uuid) {
        let result = sqlx::query(
            "UPDATE storage_s3_keys SET last_used_at = now()
             WHERE id = $1 AND revoked_at IS NULL
               AND (last_used_at IS NULL OR last_used_at < now() - interval '60 seconds')",
        )
        .bind(id)
        .execute(&self.pool)
        .await;
        if let Err(error) = result {
            tracing::warn!(key_id = %id, %error, "could not stamp the S3 key's last use");
        }
    }
}
