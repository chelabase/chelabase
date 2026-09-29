//! Neon endpoints, roles and databases: what the proxy, compute_ctl and the storage
//! controller ask the orchestrator's internal listener about.

use std::time::Duration;

use crate::{DatabaseStatus, MetadataClient, MetadataError};
use cheladb_types::DatabaseId;
use sqlx::{Postgres, Row, Transaction};

/// The first key of every endpoint lock (`pg_advisory_xact_lock(int, int)`), so they
/// can't collide with other advisory locks (sqlx's migration lock uses the one-key form).
const ENDPOINT_LOCK_CLASS: i32 = 0x6e65_6f6e; // "neon"
/// How long a locker queues for an endpoint's lock before giving up. Under the worker's 8 s
/// `RESET_DEADLINE`, so a reset that waits its full turn still ends with a clear error and
/// some time to spare; reconcile gives up after it too and retries on its next pass.
pub const ENDPOINT_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
/// Postgres' `lock_not_available`: the `lock_timeout` ran out.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// An endpoint lock from [`MetadataClient::lock_endpoint`]. Dropping it releases the lock:
/// the transaction it lives in rolls back when its connection goes back to the pool.
pub struct EndpointLock {
    _tx: Transaction<'static, Postgres>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeonEndpoint {
    pub id: String,
    pub tenant_id: String,
    pub timeline_id: String,
    pub compute_token_sha256: String,
}

/// Everything `create_endpoint_for_database` writes.
#[derive(Debug, Clone, Copy)]
pub struct NewEndpoint<'a> {
    pub id: &'a str,
    pub tenant_id: &'a str,
    pub timeline_id: &'a str,
    pub compute_token_sha256: &'a str,
    pub owner_role: &'a str,
    pub owner_verifier: &'a str,
    pub database_name: &'a str,
}

/// An endpoint and the database linked to it, as seen by the reconcile loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointState {
    pub id: String,
    pub tenant_id: String,
    pub database_id: Option<DatabaseId>,
    pub database_status: Option<DatabaseStatus>,
    pub needs_reconfigure: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub struct NeonRole {
    pub name: String,
    pub scram_verifier: String,
}

impl std::fmt::Debug for NeonRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NeonRole")
            .field("name", &self.name)
            .field("scram_verifier", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_verifier() {
        let role = NeonRole {
            name: "postgres".into(),
            scram_verifier: "SCRAM-SHA-256$secret".into(),
        };
        let shown = format!("{role:?}");
        assert!(shown.contains("postgres") && shown.contains("<redacted>"));
        assert!(!shown.contains("SCRAM"));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeonDatabase {
    pub name: String,
    pub owner: String,
}

impl MetadataClient {
    pub async fn neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        let row: Option<(String, String, String, String)> = sqlx::query_as(
            "SELECT id, tenant_id, timeline_id, compute_token_sha256 FROM neon_endpoints WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(id, tenant_id, timeline_id, compute_token_sha256)| NeonEndpoint {
                id,
                tenant_id,
                timeline_id,
                compute_token_sha256,
            },
        ))
    }

    /// `None` for an unknown endpoint; `Some(None)` for a known endpoint without that role.
    pub async fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT r.scram_verifier FROM neon_endpoints e
             LEFT JOIN neon_roles r ON r.endpoint_id = e.id AND r.name = $2
             WHERE e.id = $1",
        )
        .bind(endpoint_id)
        .bind(role)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(verifier,)| verifier))
    }

    pub async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, scram_verifier FROM neon_roles WHERE endpoint_id = $1 ORDER BY name",
        )
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, scram_verifier)| NeonRole {
                name,
                scram_verifier,
            })
            .collect())
    }

    pub async fn neon_databases(
        &self,
        endpoint_id: &str,
    ) -> Result<Vec<NeonDatabase>, MetadataError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, owner FROM neon_databases WHERE endpoint_id = $1 ORDER BY name",
        )
        .bind(endpoint_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, owner)| NeonDatabase { name, owner })
            .collect())
    }

    pub async fn neon_endpoints_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        Ok(
            sqlx::query_scalar("SELECT id FROM neon_endpoints WHERE tenant_id = $1 ORDER BY id")
                .bind(tenant_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }

    pub async fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        Ok(
            sqlx::query_scalar("SELECT id FROM neon_endpoints WHERE timeline_id = $1 ORDER BY id")
                .bind(timeline_id)
                .fetch_all(&self.pool)
                .await?,
        )
    }
}

impl MetadataClient {
    /// Waits until this is the only holder of `endpoint_id`'s lock, across every process on
    /// this metadata database. A transaction-level advisory lock in its own transaction, so
    /// nothing can return a still-locked connection to the pool. Postgres queues the waiters
    /// first come, first served; each holds a lock-pool connection while it waits. After
    /// [`ENDPOINT_LOCK_TIMEOUT`] it gives up with [`MetadataError::EndpointLockTimeout`].
    pub async fn lock_endpoint(&self, endpoint_id: &str) -> Result<EndpointLock, MetadataError> {
        let mut tx = self.locks.begin().await?;
        // `SET LOCAL lock_timeout`, as a function so the value can be bound.
        sqlx::query("SELECT set_config('lock_timeout', $1, true)")
            .bind(format!("{}ms", ENDPOINT_LOCK_TIMEOUT.as_millis()))
            .execute(&mut *tx)
            .await?;
        let locked = sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
            .bind(ENDPOINT_LOCK_CLASS)
            .bind(endpoint_id)
            .execute(&mut *tx)
            .await;
        match locked {
            Ok(_) => Ok(EndpointLock { _tx: tx }),
            Err(sqlx::Error::Database(err))
                if err.code().as_deref() == Some(LOCK_NOT_AVAILABLE) =>
            {
                Err(MetadataError::EndpointLockTimeout(endpoint_id.to_string()))
            }
            Err(err) => Err(err.into()),
        }
    }

    /// One transaction: endpoint, role, database, then link. False (and nothing written) when the
    /// database is deleting/deleted/missing or already linked.
    pub async fn create_endpoint_for_database(
        &self,
        id: DatabaseId,
        ep: &NewEndpoint<'_>,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "SELECT 1 FROM databases
             WHERE id = $1 AND status NOT IN ('deleting', 'deleted') AND endpoint_id IS NULL
             FOR UPDATE",
        )
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await?;
        if claimed.is_none() {
            return Ok(false);
        }
        sqlx::query(
            "INSERT INTO neon_endpoints (id, tenant_id, timeline_id, compute_token_sha256)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(ep.id)
        .bind(ep.tenant_id)
        .bind(ep.timeline_id)
        .bind(ep.compute_token_sha256)
        .execute(&mut *tx)
        .await?;
        sqlx::query("INSERT INTO neon_roles (endpoint_id, name, scram_verifier) VALUES ($1, $2, $3)")
            .bind(ep.id)
            .bind(ep.owner_role)
            .bind(ep.owner_verifier)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO neon_databases (endpoint_id, name, owner) VALUES ($1, $2, $3)")
            .bind(ep.id)
            .bind(ep.database_name)
            .bind(ep.owner_role)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE databases SET endpoint_id = $2, updated_at = now() WHERE id = $1")
            .bind(id.as_uuid())
            .bind(ep.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Roles and databases cascade; databases.endpoint_id becomes NULL. Missing is fine.
    pub async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        sqlx::query("DELETE FROM neon_endpoints WHERE id = $1")
            .bind(endpoint_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// One transaction: sealed password (only while ready and linked to endpoint_id), the role's
    /// verifier, needs_reconfigure = true. False (nothing written) otherwise.
    pub async fn replace_owner_secret(
        &self,
        id: DatabaseId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed_password: &str,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let sealed = sqlx::query(
            "UPDATE databases SET owner_password_enc = $3, updated_at = now()
             WHERE id = $1 AND endpoint_id = $2 AND status = 'ready'",
        )
        .bind(id.as_uuid())
        .bind(endpoint_id)
        .bind(sealed_password)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if sealed != 1 {
            return Ok(false);
        }
        let verified = sqlx::query(
            "UPDATE neon_roles SET scram_verifier = $3 WHERE endpoint_id = $1 AND name = $2",
        )
        .bind(endpoint_id)
        .bind(role)
        .bind(verifier)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if verified != 1 {
            return Ok(false);
        }
        sqlx::query("UPDATE neon_endpoints SET needs_reconfigure = true WHERE id = $1")
            .bind(endpoint_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Clears the flag only while `role`'s verifier still equals `applied_verifier`. True when cleared.
    pub async fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        role: &str,
        applied_verifier: &str,
    ) -> Result<bool, MetadataError> {
        let cleared = sqlx::query(
            // FOR SHARE on the role row makes the verifier comparison wait for, and then see,
            // a concurrent reset's commit (READ COMMITTED would otherwise compare against the
            // statement's old snapshot). Lock order matches replace_owner_secret: role, endpoint.
            "WITH applied AS (
                 SELECT 1 FROM neon_roles
                 WHERE endpoint_id = $1 AND name = $2 AND scram_verifier = $3
                 FOR SHARE
             )
             UPDATE neon_endpoints SET needs_reconfigure = false
             WHERE id = $1 AND needs_reconfigure AND EXISTS (SELECT 1 FROM applied)",
        )
        .bind(endpoint_id)
        .bind(role)
        .bind(applied_verifier)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(cleared == 1)
    }

    pub async fn replace_compute_token(
        &self,
        endpoint_id: &str,
        sha256_hex: &str,
    ) -> Result<(), MetadataError> {
        sqlx::query("UPDATE neon_endpoints SET compute_token_sha256 = $2 WHERE id = $1")
            .bind(endpoint_id)
            .bind(sha256_hex)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Every endpoint, left-joined to the database linked to it.
    pub async fn list_endpoints_for_reconcile(&self) -> Result<Vec<EndpointState>, MetadataError> {
        let rows = sqlx::query(
            "SELECT e.id, e.tenant_id, e.needs_reconfigure, d.id AS database_id, d.status
             FROM neon_endpoints e
             LEFT JOIN databases d ON d.endpoint_id = e.id
             ORDER BY e.id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                let status: Option<String> = r.get("status");
                Ok(EndpointState {
                    id: r.get("id"),
                    tenant_id: r.get("tenant_id"),
                    database_id: r.get::<Option<uuid::Uuid>, _>("database_id").map(DatabaseId),
                    database_status: status.as_deref().map(DatabaseStatus::parse).transpose()?,
                    needs_reconfigure: r.get("needs_reconfigure"),
                })
            })
            .collect()
    }
}
