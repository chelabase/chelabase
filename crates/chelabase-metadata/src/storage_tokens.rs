//! Object storage's control-plane state (m6): the one-time upload tokens and the data key
//! liveness its signed URLs re-check. Single statements, one table each (`data_api_keys` is
//! only read).

use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::postgres::PgRow;
use uuid::Uuid;

use crate::{DataKeyLookup, MetadataClient, MetadataError};
use chelabase_types::{BranchId, ProjectId};

/// A live key (`k`, on branch `b`), as [`MetadataClient::lookup_data_key`] decides.
macro_rules! live_key {
    () => {
        "k.revoked_at IS NULL AND b.kind = 'normal' AND b.status NOT IN ('deleting', 'deleted')"
    };
}

fn lookup(r: &PgRow) -> Result<DataKeyLookup, MetadataError> {
    let kind: String = r.get("kind");
    Ok(DataKeyLookup {
        key_id: r.get("key_id"),
        branch_id: BranchId(r.get("branch_id")),
        project_id: ProjectId(r.get("project_id")),
        kind: chelabase_types::data_key::DataKeyKind::parse(&kind)
            .ok_or_else(|| MetadataError::InvalidStatus(format!("data key kind {kind}")))?,
        role: r.get("role"),
        databases: r.get("databases"),
        role_oid: r.get("role_oid"),
    })
}

impl MetadataClient {
    /// The key with this id while it is live (as [`MetadataClient::lookup_data_key`] decides:
    /// not revoked, on a `normal` branch that isn't deleting or deleted).
    pub async fn live_data_key(
        &self,
        key_id: Uuid,
    ) -> Result<Option<DataKeyLookup>, MetadataError> {
        sqlx::query(concat!(
            "SELECT k.id AS key_id, k.branch_id, d.project_id, k.kind, k.role, k.databases, k.role_oid
             FROM data_api_keys k
             JOIN branches b ON b.id = k.branch_id
             JOIN databases d ON d.id = b.database_id
             WHERE k.id = $1 AND ",
            live_key!()
        ))
            .bind(key_id)
            .fetch_optional(&self.pool)
            .await?
            .as_ref()
            .map(lookup)
            .transpose()
    }

    /// Records a minted upload token. False when the id is taken (never reused) or the key isn't
    /// live. Prunes rows a day past their expiry first: longer than any clock skew between
    /// replicas, and a token is refused by its own `exp` before its row is read, so a pruned
    /// row can never be replayed.
    pub async fn insert_upload_token(
        &self,
        token_id: Uuid,
        key_id: Uuid,
        claims: &serde_json::Value,
        expires_at: DateTime<Utc>,
    ) -> Result<bool, MetadataError> {
        sqlx::query(
            "DELETE FROM storage_upload_tokens WHERE expires_at < now() - interval '1 day'",
        )
        .execute(&self.pool)
        .await?;
        let inserted = sqlx::query(concat!(
            "INSERT INTO storage_upload_tokens (token_id, key_id, claims, expires_at)
             SELECT $1, k.id, $3, $4 FROM data_api_keys k JOIN branches b ON b.id = k.branch_id
             WHERE k.id = $2 AND ",
            live_key!(),
            " ON CONFLICT (token_id) DO NOTHING"
        ))
        .bind(token_id)
        .bind(key_id)
        .bind(claims)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(inserted.rows_affected() == 1)
    }

    /// Spends an upload token: once, before its expiry, while its key is live. The key as it is
    /// now (its current role) and the minting request's claims; `None` for an unknown, spent or
    /// expired token, or a dead key. One statement, so two replicas can't both spend it.
    pub async fn consume_upload_token(
        &self,
        token_id: Uuid,
    ) -> Result<Option<(DataKeyLookup, serde_json::Value)>, MetadataError> {
        let row = sqlx::query(concat!(
            "UPDATE storage_upload_tokens t SET used_at = now()
             FROM data_api_keys k
             JOIN branches b ON b.id = k.branch_id
             JOIN databases d ON d.id = b.database_id
             WHERE t.token_id = $1 AND t.used_at IS NULL AND t.expires_at > now()
               AND k.id = t.key_id AND ",
            live_key!(),
            " RETURNING k.id AS key_id, k.branch_id, d.project_id, k.kind, k.role, k.databases, k.role_oid, t.claims"
        ))
        .bind(token_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| Ok((lookup(&r)?, r.get("claims")))).transpose()
    }
}
