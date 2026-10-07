//! Data API metadata: keys, the authenticator password and the per-database settings the REST
//! service reads.
//!
//! Lock order: see `branches.rs`. [`MetadataClient::rotate_data_key`] spans more than one
//! statement; it locks the database row, then the branch row, then touches key rows. Secret key
//! creation and rotation also take the endpoint row `FOR SHARE` after the branch row (database,
//! branch, endpoint: `begin_ddl` takes branch, endpoint). The other writes are single statements
//! on one table.
//!
//! # Secret keys and their roles
//!
//! **Invariant:** a secret key is bound to its role's identity on the compute
//! (`pg_roles.oid`, the `role_oid` column), not to the role's name. A key whose OID no longer
//! exists, or whose name is now held by a different role, is dead: it never runs as anyone.
//!
//! How each side keeps it:
//! - Creation: the worker checks the role on the compute (any role, with or without a
//!   password, so with or without a `neon_roles` row) and answers its OID;
//!   [`MetadataClient::create_secret_key`] stores the key with it.
//! - Use: rest's preamble sets the role only while the role named `role` has `role_oid`, in
//!   the same expression as the `SET ROLE` (no window, no cache). A role dropped, or dropped
//!   and created again, between the check and the store (or at any time later) leaves a key
//!   that is refused on every request.
//! - Rotation re-checks the role on the compute; a different OID than the old key's is
//!   [`MetadataError::RoleRecreated`], nothing written. The new key keeps the old key's OID,
//!   and its role is the old row's current name (read under the lock, so it follows a rename).
//! - The resync revokes keys whose OID is gone, renames keys whose OID now has another name (a
//!   rename the receiver didn't see), and fills the OID of keys made before the column existed.
//! - The DDL receiver (endpoint row `FOR UPDATE`, key writes take it `FOR SHARE`) refuses a
//!   DROP or recreate of a role with live keys and moves keys with every rename: the
//!   friendly path, so a key rarely goes dead in the first place.

use std::fmt;

use chelabase_types::{BranchId, ProjectId};
use chelabase_types::data_key::DataKeyKind;
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
    /// The databases the key may reach; `None` is every served database.
    pub databases: Option<Vec<String>>,
    /// A secret key's role identity (`pg_roles.oid`) from the worker's check; `None` for the
    /// public key.
    pub role_oid: Option<i64>,
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
            .field("databases", &self.databases)
            .field("role_oid", &self.role_oid)
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
    /// The databases the key may reach; `None` is every served database.
    pub databases: Option<Vec<String>>,
    /// A secret key's role identity; `None` for the public key and keys not backfilled yet.
    pub role_oid: Option<i64>,
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
            .field("databases", &self.databases)
            .field("role_oid", &self.role_oid)
            .finish()
    }
}

/// What the gateway learns from a key's hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataKeyLookup {
    pub key_id: Uuid,
    pub branch_id: BranchId,
    /// The project the key's database belongs to.
    pub project_id: ProjectId,
    pub kind: DataKeyKind,
    pub role: String,
    /// The databases the key may reach; `None` is every served database.
    pub databases: Option<Vec<String>>,
    /// The role identity rest holds the key to; `None` checks the name only.
    pub role_oid: Option<i64>,
}

/// One repair the resync makes to a live secret key from the compute's `pg_roles`. Each is
/// applied only while the row still looks as the resync saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyRoleFix {
    /// The key's OID is gone, or (no OID yet) its name is: the role is dropped.
    Revoke { id: Uuid, role: String, oid: Option<i64> },
    /// The key's OID now has another name (a rename the DDL receiver didn't see).
    Follow { id: Uuid, from: String, to: String, oid: i64 },
    /// A key made before OIDs were stored: its name's current OID.
    Backfill { id: Uuid, role: String, oid: i64 },
}

/// A database's data API settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataApiSettings {
    pub max_rows: i32,
    pub statement_timeout_ms: i32,
    pub max_connections: i32,
}

/// A database the data API serves, with the schema requests use when they name none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedDatabase {
    pub name: String,
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
    /// The branch's databases in state `on`, main first.
    pub databases: Vec<ServedDatabase>,
}

const KEY_COLUMNS: &str = "id, branch_id, kind, role, name, prefix, sealed, created_at, \
     last_used_at, revoked_at, databases, role_oid";

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
        databases: r.get("databases"),
        role_oid: r.get("role_oid"),
    })
}

const INSERT_KEY: &str =
    "INSERT INTO data_api_keys
         (id, branch_id, kind, role, name, prefix, sha256, sealed, databases, role_oid)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)";

/// A second live public key on a branch is a [`MetadataError::Conflict`]; anything else passes.
fn map_insert_error(err: sqlx::Error) -> MetadataError {
    match err {
        sqlx::Error::Database(e) if e.constraint() == Some(ONE_LIVE_PUBLIC) => {
            MetadataError::Conflict
        }
        err => err.into(),
    }
}

/// How long key creation and rotation wait for the endpoint row before giving up.
const ENDPOINT_WAIT: &str = "5s";

/// Takes the branch row (`FOR NO KEY UPDATE`) and then its endpoint row (`FOR SHARE`), with a
/// `lock_timeout`. A missing branch is `RowNotFound`.
async fn lock_endpoint_of_branch(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    branch: BranchId,
) -> Result<(), MetadataError> {
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(ENDPOINT_WAIT)
        .execute(&mut **tx)
        .await?;
    let endpoint: Option<Option<String>> =
        sqlx::query_scalar("SELECT endpoint_id FROM branches WHERE id = $1 FOR NO KEY UPDATE")
            .bind(branch.as_uuid())
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| crate::ddl::lock_timeout(e, &branch.to_string()))?;
    let Some(endpoint) = endpoint else {
        return Err(sqlx::Error::RowNotFound.into());
    };
    if let Some(endpoint) = &endpoint {
        sqlx::query("SELECT id FROM neon_endpoints WHERE id = $1 FOR SHARE")
            .bind(endpoint)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|e| crate::ddl::lock_timeout(e, endpoint))?;
    }
    Ok(())
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

    /// Stores a secret key, with the role OID the worker's check found (the key is bound to
    /// that identity; see the module docs). The role needs no `neon_roles` row.
    ///
    /// Lock order: branch row (`FOR NO KEY UPDATE`, as [`Self::rotate_data_key`] and
    /// `begin_ddl` take it), then the endpoint row `FOR SHARE`: the DDL receiver holds the same
    /// row `FOR UPDATE` while it reads `live_secret_keys` and drops or renames the role, so a
    /// DROP forwarded after the store sees the key and is refused, and a rename moves it. A
    /// DROP or recreate the store didn't wait for leaves a key bound to an OID that is gone:
    /// dead, never a key of the next role under the name. A lock wait over 5 s is
    /// [`MetadataError::EndpointLockTimeout`].
    ///
    /// Public keys keep [`Self::insert_data_key`]: their role is `anon`, which no DDL can drop,
    /// and branch creation stores one before the endpoint is linked.
    pub async fn create_secret_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_endpoint_of_branch(&mut tx, key.branch_id).await?;
        bind_new_key(sqlx::query(INSERT_KEY), key, key.branch_id)
            .execute(&mut *tx)
            .await
            .map_err(map_insert_error)?;
        tx.commit().await?;
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
            "SELECT k.id, k.branch_id, d.project_id, k.kind, k.role, k.databases, k.role_oid
             FROM data_api_keys k
             JOIN branches b ON b.id = k.branch_id
             JOIN databases d ON d.id = b.database_id
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
                project_id: ProjectId(r.get("project_id")),
                kind: parse_kind(r.get("kind"))?,
                role: r.get("role"),
                databases: r.get("databases"),
                role_oid: r.get("role_oid"),
            })
        })
        .transpose()
    }

    /// Applies the resync's [`KeyRoleFix`]es to `branch`'s live secret keys in one
    /// transaction, each only while the row still has the role (and OID) the resync saw: a
    /// key renamed, rotated or revoked meanwhile is left alone. Returns how many rows changed.
    pub async fn fix_key_roles(
        &self,
        branch: BranchId,
        fixes: &[KeyRoleFix],
    ) -> Result<u64, MetadataError> {
        if fixes.is_empty() {
            return Ok(0);
        }
        let mut tx = self.pool.begin().await?;
        let mut changed = 0;
        for fix in fixes {
            let query = match fix {
                KeyRoleFix::Revoke { id, role, oid } => sqlx::query(
                    "UPDATE data_api_keys SET revoked_at = now()
                     WHERE id = $1 AND branch_id = $2 AND kind = 'secret' AND revoked_at IS NULL
                       AND role = $3 AND role_oid IS NOT DISTINCT FROM $4",
                )
                .bind(*id)
                .bind(branch.as_uuid())
                .bind(role)
                .bind(*oid),
                KeyRoleFix::Follow { id, from, to, oid } => sqlx::query(
                    "UPDATE data_api_keys SET role = $4
                     WHERE id = $1 AND branch_id = $2 AND kind = 'secret' AND revoked_at IS NULL
                       AND role = $3 AND role_oid = $5",
                )
                .bind(*id)
                .bind(branch.as_uuid())
                .bind(from)
                .bind(to)
                .bind(*oid),
                KeyRoleFix::Backfill { id, role, oid } => sqlx::query(
                    "UPDATE data_api_keys SET role_oid = $4
                     WHERE id = $1 AND branch_id = $2 AND kind = 'secret' AND revoked_at IS NULL
                       AND role = $3 AND role_oid IS NULL",
                )
                .bind(*id)
                .bind(branch.as_uuid())
                .bind(role)
                .bind(*oid),
            };
            changed += query.execute(&mut *tx).await?.rows_affected();
        }
        tx.commit().await?;
        Ok(changed)
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
    /// Public keys only: a secret `old` is false, as for an unknown key; secret keys go through
    /// [`Self::rotate_secret_key`], which also holds the role in place.
    pub async fn rotate_data_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        self.rotate_in_tx(branch, old, new, false).await
    }

    /// [`Self::rotate_data_key`] for a live secret key. The endpoint row is held `FOR SHARE`,
    /// so a concurrent role DROP, recreate or rename (the DDL receiver holds it `FOR UPDATE`)
    /// either finishes before this runs or sees the new key. The new key's role is the revoked
    /// row's current one (a rename may have moved it since the caller read it), and its OID the
    /// old key's: the identity carries over. `new.role_oid` is what the caller's check found;
    /// a different OID than the old key's means the role was dropped and created again, so
    /// [`MetadataError::RoleRecreated`], the revoke rolled back. An old key without an OID
    /// takes the checked one. False for a public key.
    pub async fn rotate_secret_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        self.rotate_in_tx(branch, old, new, true).await
    }

    /// Lock order (the DDL receiver's, `ddl.rs::begin_ddl`, is a suffix of it): database row,
    /// branch row (`FOR NO KEY UPDATE`), endpoint row (`FOR SHARE`, secret keys only), then
    /// key rows.
    async fn rotate_in_tx(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
        secret: bool,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        lock_database_of_branch(&mut tx, branch).await?;
        if secret {
            lock_endpoint_of_branch(&mut tx, branch).await?;
        } else {
            sqlx::query("SELECT 1 FROM branches WHERE id = $1 FOR NO KEY UPDATE")
                .bind(branch.as_uuid())
                .execute(&mut *tx)
                .await?;
        }
        // The new key reaches what the old one did: its list is copied, whatever `new` carries.
        // A secret key's role is the old row's now, under the lock.
        let revoked: Option<(Option<Vec<String>>, String, Option<i64>)> = sqlx::query_as(
            "UPDATE data_api_keys SET revoked_at = now()
             WHERE id = $1 AND branch_id = $2 AND revoked_at IS NULL
               AND kind = CASE WHEN $3 THEN 'secret' ELSE 'public' END
             RETURNING databases, role, role_oid",
        )
        .bind(old)
        .bind(branch.as_uuid())
        .bind(secret)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((databases, role, stored_oid)) = revoked else {
            return Ok(false);
        };
        let role_oid = if secret {
            if let (Some(stored), Some(checked)) = (stored_oid, new.role_oid)
                && stored != checked
            {
                return Err(MetadataError::RoleRecreated(role));
            }
            stored_oid.or(new.role_oid)
        } else {
            None
        };
        let new = NewDataKey {
            databases,
            role: if secret { role } else { new.role.clone() },
            role_oid,
            ..new.clone()
        };
        bind_new_key(sqlx::query(INSERT_KEY), &new, branch)
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

    /// Replaces a live key's database list (`None` is every served database). False for an
    /// unknown or revoked key, or one of another branch. The list's bounds (1 to 20 names) are
    /// the column's CHECK; callers validate first for a friendly message.
    pub async fn set_key_databases(
        &self,
        branch: BranchId,
        key_id: Uuid,
        databases: Option<Vec<String>>,
    ) -> Result<bool, MetadataError> {
        let updated = sqlx::query(
            "UPDATE data_api_keys SET databases = $3
             WHERE id = $1 AND branch_id = $2 AND revoked_at IS NULL",
        )
        .bind(key_id)
        .bind(branch.as_uuid())
        .bind(databases)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
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
                    d.database_name, d.max_rows, d.statement_timeout_ms, d.max_connections
             FROM branches b
             JOIN databases d ON d.id = b.database_id
             WHERE b.id = $1",
        )
        .bind(branch.as_uuid())
        .fetch_optional(&self.pool)
        .await?;
        let Some(r) = row else {
            return Ok(None);
        };
        let databases = self.served_databases(branch).await?;
        Ok(Some(BranchConnectionRow {
            branch_id: BranchId(r.get("id")),
            status: BranchStatus::parse(r.get("status"))?,
            endpoint_id: r.get("endpoint_id"),
            database_name: r.get("database_name"),
            authenticator_password_enc: r.get("authenticator_password_enc"),
            settings: DataApiSettings {
                max_rows: r.get("max_rows"),
                statement_timeout_ms: r.get("statement_timeout_ms"),
                max_connections: r.get("max_connections"),
            },
            databases,
        }))
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
        .bind(key.databases.as_deref())
        .bind(key.role_oid)
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
            databases: None,
            role_oid: None,
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
            databases: None,
            role_oid: None,
        };
        assert!(!format!("{record:?}").contains("SECRETSEALED"));
    }
}
