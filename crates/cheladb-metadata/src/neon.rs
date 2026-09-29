//! Neon endpoints, roles and databases: what the proxy, compute_ctl and the storage
//! controller ask the orchestrator's internal listener about.

use crate::{MetadataClient, MetadataError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeonEndpoint {
    pub id: String,
    pub tenant_id: String,
    pub timeline_id: String,
    pub compute_token_sha256: String,
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
