//! The DDL receiver's transaction: one endpoint's roles and databases, changed from SQL.
//!
//! [`MetadataClient::begin_ddl`] locks the endpoint row (the same lock every role writer
//! takes), so a receiver call, an API role write and a resync queue instead of interleaving.
//! The transaction's `lock_timeout` is 2 s: the compute's curl gives up after 3 s and retries,
//! so the receiver answers "try again" first. Dropping a [`DdlTx`] without [`DdlTx::commit`]
//! rolls everything back.

use chelabase_types::BranchId;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::neon::{RoleCopy, copy_from_row};
use crate::{DatabaseOrigin, MetadataClient, MetadataError};

/// Postgres' `lock_not_available`: the `lock_timeout` ran out.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// An open transaction holding one endpoint's row lock.
pub struct DdlTx {
    tx: Transaction<'static, Postgres>,
    endpoint_id: String,
    branch_id: Option<BranchId>,
    owner: String,
    main_database: Option<String>,
    /// Something was written: `commit` bumps `roles_version`.
    wrote: bool,
}

/// A lock timeout becomes [`MetadataError::EndpointLockTimeout`]; other errors pass through.
pub(crate) fn lock_timeout(err: sqlx::Error, endpoint_id: &str) -> MetadataError {
    match err {
        sqlx::Error::Database(e) if e.code().as_deref() == Some(LOCK_NOT_AVAILABLE) => {
            MetadataError::EndpointLockTimeout(endpoint_id.to_string())
        }
        other => other.into(),
    }
}

impl MetadataClient {
    /// The live branch linked to `endpoint_id`, read without the endpoint's lock.
    pub async fn endpoint_branch(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<BranchId>, MetadataError> {
        let branch: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM branches
             WHERE endpoint_id = $1 AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(endpoint_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(branch.map(BranchId))
    }

    /// Opens the receiver's transaction for `endpoint_id`: `lock_timeout` of 2 s, the linked
    /// branch's row (`FOR NO KEY UPDATE`, first: the branch row comes before the endpoint row
    /// in the lock order, and `set_owner_verifier` writes to it), then the endpoint's row
    /// (`FOR UPDATE`). Reads the owner's name, the linked branch (`branches.endpoint_id`, not
    /// deleted) and its main database. `None` for an unknown endpoint. A lock timeout is
    /// [`MetadataError::EndpointLockTimeout`].
    pub async fn begin_ddl(&self, endpoint_id: &str) -> Result<Option<DdlTx>, MetadataError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT set_config('lock_timeout', '2s', true)")
            .execute(&mut *tx)
            .await?;
        let branch: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM branches
             WHERE endpoint_id = $1 AND status NOT IN ('deleting', 'deleted')
             FOR NO KEY UPDATE",
        )
        .bind(endpoint_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| lock_timeout(e, endpoint_id))?;
        let locked: Option<String> =
            sqlx::query_scalar("SELECT id FROM neon_endpoints WHERE id = $1 FOR UPDATE")
                .bind(endpoint_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|e| lock_timeout(e, endpoint_id))?;
        if locked.is_none() {
            return Ok(None);
        }
        let owner: String = sqlx::query_scalar(
            "SELECT name FROM neon_roles WHERE endpoint_id = $1 AND origin = 'owner'",
        )
        .bind(endpoint_id)
        .fetch_one(&mut *tx)
        .await?;
        let main_database: Option<String> = sqlx::query_scalar(
            "SELECT name FROM neon_databases WHERE endpoint_id = $1 ORDER BY name LIMIT 1",
        )
        .bind(endpoint_id)
        .fetch_optional(&mut *tx)
        .await?;
        Ok(Some(DdlTx {
            tx,
            endpoint_id: endpoint_id.to_string(),
            branch_id: branch.map(BranchId),
            owner,
            main_database,
            wrote: false,
        }))
    }
}

impl DdlTx {
    /// The branch linked to the endpoint, if any.
    pub fn branch_id(&self) -> Option<BranchId> {
        self.branch_id
    }

    /// The owner role's name.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The recorded name of the branch's main database.
    pub fn main_database(&self) -> Option<&str> {
        self.main_database.as_deref()
    }

    /// Any row of the endpoint (the owner's included), with its secrets.
    pub async fn role(&mut self, name: &str) -> Result<Option<RoleCopy>, MetadataError> {
        let row = sqlx::query(
            "SELECT r.name, r.origin, r.scram_verifier, r.password_enc, r.login, r.level,
                    r.api_owner_level, r.pending, r.attributes_pending, r.created_at, r.inherit,
                    r.connection_limit,
                    r.valid_until, r.bypass_rls, r.member_of
             FROM neon_roles r WHERE r.endpoint_id = $1 AND r.name = $2",
        )
        .bind(&self.endpoint_id)
        .bind(name)
        .fetch_optional(&mut *self.tx)
        .await?;
        row.as_ref().map(copy_from_row).transpose()
    }

    /// Rows of origin `sql` or `api` with a verifier: what the role cap counts (R5).
    pub async fn copy_count(&mut self) -> Result<u32, MetadataError> {
        let n = crate::neon::copy_count(&mut self.tx, &self.endpoint_id).await?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// The branch's data keys (not revoked) whose role is `role`: `(id, name)`.
    pub async fn live_secret_keys(
        &mut self,
        role: &str,
    ) -> Result<Vec<(Uuid, String)>, MetadataError> {
        let Some(branch) = self.branch_id else {
            return Ok(Vec::new());
        };
        Ok(sqlx::query_as(
            "SELECT id, name FROM data_api_keys
             WHERE branch_id = $1 AND role = $2 AND revoked_at IS NULL
             ORDER BY created_at, id",
        )
        .bind(branch.as_uuid())
        .bind(role)
        .fetch_all(&mut *self.tx)
        .await?)
    }

    /// Inserts a copy (origin `sql`, login true, level plain) or, for an existing row,
    /// replaces its verifier and drops its sealed password (the password was typed in SQL).
    /// `forwarded_at = now()` either way. A recreated role's old row is removed first by the
    /// receiver, so it starts from a fresh one.
    pub async fn upsert_sql_copy(
        &mut self,
        name: &str,
        verifier: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query(
            "INSERT INTO neon_roles (endpoint_id, name, scram_verifier, origin, forwarded_at)
             VALUES ($1, $2, $3, 'sql', now())
             ON CONFLICT (endpoint_id, name) DO UPDATE
                 SET scram_verifier = EXCLUDED.scram_verifier, password_enc = NULL,
                     password_unreadable = false, forwarded_at = now()
                 WHERE neon_roles.origin <> 'owner'",
        )
        .bind(&self.endpoint_id)
        .bind(name)
        .bind(verifier)
        .execute(&mut *self.tx)
        .await?;
        self.wrote = true;
        Ok(())
    }

    /// A forwarded `LOGIN`/`NOLOGIN`: the non-owner row's `login` flag, with `forwarded_at =
    /// now()` (the resync leaves it alone for 10 s). False when there is no such row.
    pub async fn set_login(&mut self, name: &str, login: bool) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE neon_roles SET login = $3, forwarded_at = now()
             WHERE endpoint_id = $1 AND name = $2 AND origin <> 'owner'",
        )
        .bind(&self.endpoint_id)
        .bind(name)
        .bind(login)
        .execute(&mut *self.tx)
        .await?
        .rows_affected();
        self.wrote |= updated > 0;
        Ok(updated > 0)
    }

    /// The owner row's verifier and `forwarded_at`; the branch's `owner_password_enc` becomes
    /// NULL (the sealed password no longer matches).
    pub async fn set_owner_verifier(&mut self, verifier: &str) -> Result<(), MetadataError> {
        sqlx::query(
            "UPDATE neon_roles SET scram_verifier = $2, password_unreadable = false, forwarded_at = now()
             WHERE endpoint_id = $1 AND origin = 'owner'",
        )
        .bind(&self.endpoint_id)
        .bind(verifier)
        .execute(&mut *self.tx)
        .await?;
        if let Some(branch) = self.branch_id {
            sqlx::query(
                "UPDATE branches SET owner_password_enc = NULL, updated_at = now() WHERE id = $1",
            )
            .bind(branch.as_uuid())
            .execute(&mut *self.tx)
            .await
            .map_err(|e| lock_timeout(e, &self.endpoint_id))?;
        }
        self.wrote = true;
        Ok(())
    }

    /// The sealed password of `name` couldn't be resealed: it's dropped and the row shows
    /// [`crate::PasswordState::Unreadable`]. Does not touch `forwarded_at`. No such row is a no-op.
    pub async fn mark_password_unreadable(&mut self, name: &str) -> Result<(), MetadataError> {
        let marked = sqlx::query(
            "UPDATE neon_roles SET password_enc = NULL, password_unreadable = true
             WHERE endpoint_id = $1 AND name = $2",
        )
        .bind(&self.endpoint_id)
        .bind(name)
        .execute(&mut *self.tx)
        .await?
        .rows_affected();
        self.wrote |= marked > 0;
        Ok(())
    }

    /// Deletes a non-owner row. False when there was none.
    pub async fn remove_copy(&mut self, name: &str) -> Result<bool, MetadataError> {
        let deleted = sqlx::query(
            "DELETE FROM neon_roles WHERE endpoint_id = $1 AND name = $2 AND origin <> 'owner'",
        )
        .bind(&self.endpoint_id)
        .bind(name)
        .execute(&mut *self.tx)
        .await?
        .rows_affected();
        self.wrote |= deleted > 0;
        Ok(deleted > 0)
    }

    /// Moves a row (keeping its origin and level) to `new`, replaces its sealed password with
    /// `resealed` (`None` leaves it NULL) and sets `forwarded_at = now()`. The branch's data
    /// keys with role `old` follow (R19), live and revoked alike, whether or not there is a
    /// row: a role without a password has none and can still back keys, and its live keys
    /// must not stay under a name the next `CREATE ROLE` will take. Revoked keys are inert
    /// and unlisted; moving them keeps their history with the role. They can still stay
    /// under a name that is dropped and created again (`DROP b; CREATE b`), or onto which a
    /// rename lands when all its keys are revoked: audit noise only.
    /// True when a row moved.
    pub async fn rename_copy(
        &mut self,
        old: &str,
        new: &str,
        resealed: Option<&str>,
    ) -> Result<bool, MetadataError> {
        let moved = sqlx::query(
            "UPDATE neon_roles SET name = $3, password_enc = $4,
                 password_unreadable = CASE WHEN $4 IS NULL THEN password_unreadable ELSE false END,
                 forwarded_at = now()
             WHERE endpoint_id = $1 AND name = $2",
        )
        .bind(&self.endpoint_id)
        .bind(old)
        .bind(new)
        .bind(resealed)
        .execute(&mut *self.tx)
        .await?
        .rows_affected();
        let mut keys_moved = 0;
        if let Some(branch) = self.branch_id {
            keys_moved = sqlx::query(
                "UPDATE data_api_keys SET role = $3 WHERE branch_id = $1 AND role = $2",
            )
            .bind(branch.as_uuid())
            .bind(old)
            .bind(new)
            .execute(&mut *self.tx)
            .await
            .map_err(|e| lock_timeout(e, &self.endpoint_id))?
            .rows_affected();
        }
        self.wrote |= moved > 0 || keys_moved > 0;
        Ok(moved > 0)
    }

    /// Rows of origin `main` or `sql` (R23: the system database isn't counted).
    pub async fn database_count(&mut self) -> Result<u32, MetadataError> {
        let Some(branch) = self.branch_id else {
            return Ok(0);
        };
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM branch_databases
             WHERE branch_id = $1 AND origin IN ('main', 'sql')",
        )
        .bind(branch.as_uuid())
        .fetch_one(&mut *self.tx)
        .await?;
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// Whether the branch has a `branch_databases` row named `name` (false without a branch).
    pub async fn has_database(&mut self, name: &str) -> Result<bool, MetadataError> {
        let Some(branch) = self.branch_id else {
            return Ok(false);
        };
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM branch_databases WHERE branch_id = $1 AND name = $2)",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .fetch_one(&mut *self.tx)
        .await?)
    }

    /// A new row's origin is `main` when `name` is the recorded main database's name (it came
    /// back after a drop, Q1), else `sql`. An existing row keeps its origin and takes `owner`.
    pub async fn upsert_database(&mut self, name: &str, owner: &str) -> Result<(), MetadataError> {
        let Some(branch) = self.branch_id else {
            return Ok(());
        };
        let origin = if self.main_database.as_deref() == Some(name) {
            DatabaseOrigin::Main
        } else {
            DatabaseOrigin::Sql
        };
        sqlx::query(
            "INSERT INTO branch_databases (branch_id, name, owner, origin, data_api, data_api_bootstrapped)
             VALUES ($1, $2, $3, $4, CASE WHEN $4::text = 'main' THEN 'on' ELSE 'off' END,
                     $4::text = 'main')
             ON CONFLICT (branch_id, name) DO UPDATE SET owner = EXCLUDED.owner",
        )
        .bind(branch.as_uuid())
        .bind(name)
        .bind(owner)
        .bind(origin.as_str())
        .execute(&mut *self.tx)
        .await?;
        self.stamp_databases().await?;
        self.wrote = true;
        Ok(())
    }

    /// Stamps the endpoint's `dbs_forwarded_at` (the resync leaves `branch_databases` alone for
    /// 10 s after a database DDL).
    async fn stamp_databases(&mut self) -> Result<(), MetadataError> {
        sqlx::query("UPDATE neon_endpoints SET dbs_forwarded_at = now() WHERE id = $1")
            .bind(&self.endpoint_id)
            .execute(&mut *self.tx)
            .await?;
        Ok(())
    }

    /// Renames a row, keeping its origin. False when there was none.
    pub async fn rename_database(&mut self, old: &str, new: &str) -> Result<bool, MetadataError> {
        let Some(branch) = self.branch_id else {
            return Ok(false);
        };
        let moved =
            sqlx::query("UPDATE branch_databases SET name = $3 WHERE branch_id = $1 AND name = $2")
                .bind(branch.as_uuid())
                .bind(old)
                .bind(new)
                .execute(&mut *self.tx)
                .await?
                .rows_affected();
        if moved > 0 {
            // Keys that name the database follow it (same transaction).
            sqlx::query(
                "UPDATE data_api_keys SET databases = array_replace(databases, $2, $3)
                 WHERE branch_id = $1 AND databases @> ARRAY[$2]::text[]",
            )
            .bind(branch.as_uuid())
            .bind(old)
            .bind(new)
            .execute(&mut *self.tx)
            .await?;
        }
        self.stamp_databases().await?;
        self.wrote |= moved > 0;
        Ok(moved > 0)
    }

    /// Deletes any row, the main database's included (Q1). False when there was none.
    pub async fn remove_database(&mut self, name: &str) -> Result<bool, MetadataError> {
        let Some(branch) = self.branch_id else {
            return Ok(false);
        };
        let deleted =
            sqlx::query("DELETE FROM branch_databases WHERE branch_id = $1 AND name = $2")
                .bind(branch.as_uuid())
                .bind(name)
                .execute(&mut *self.tx)
                .await?
                .rows_affected();
        if deleted > 0 {
            drop_name_from_key_lists(&mut self.tx, branch, name).await?;
        }
        self.stamp_databases().await?;
        self.wrote |= deleted > 0;
        Ok(deleted > 0)
    }

    /// `roles_version += 1` when anything was written (the compute already has what the DDL
    /// did, so `needs_reconfigure` stays as it is), then commits.
    pub async fn commit(mut self) -> Result<(), MetadataError> {
        if self.wrote {
            sqlx::query(
                "UPDATE neon_endpoints SET roles_version = roles_version + 1 WHERE id = $1",
            )
            .bind(&self.endpoint_id)
            .execute(&mut *self.tx)
            .await?;
        }
        self.tx.commit().await?;
        Ok(())
    }
}

/// A dropped database's name leaves every live key's list of the branch; a key left with none is
/// revoked (an empty list can't be stored, and NULL would mean every database).
pub(crate) async fn drop_name_from_key_lists(
    conn: &mut sqlx::PgConnection,
    branch: BranchId,
    name: &str,
) -> Result<(), MetadataError> {
    sqlx::query(
        "UPDATE data_api_keys
         SET revoked_at = CASE WHEN cardinality(array_remove(databases, $2)) = 0
                               THEN now() END,
             databases = CASE WHEN cardinality(array_remove(databases, $2)) = 0
                              THEN databases ELSE array_remove(databases, $2) END
         WHERE branch_id = $1 AND revoked_at IS NULL AND databases @> ARRAY[$2]::text[]",
    )
    .bind(branch.as_uuid())
    .bind(name)
    .execute(conn)
    .await?;
    Ok(())
}
