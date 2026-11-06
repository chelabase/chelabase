//! The metadata database's side of the bootstrap: it waits until every migration this build knows
//! is applied (the orchestrator applies them). Through `chelabase_metadata`'s client over the
//! metadata database URL: no `docker`, no `psql`.

use anyhow::{Context, Result};
use chelabase_metadata::{MetadataClient, MetadataConfig};
use std::collections::HashSet;
use std::future::Future;

use super::retry::Check;

pub trait Metadata: Send + Sync {
    /// Ready once the database answers and every migration this build knows is applied (the
    /// orchestrator applies them).
    fn migrated(&self) -> impl Future<Output = Result<Check<()>>> + Send;
}

/// The metadata migrations this build knows (the orchestrator applies the same set).
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../chelabase-metadata/migrations");

/// The real metadata database, through `chelabase_metadata`'s client.
pub struct SqlMetadata {
    client: MetadataClient,
}

impl SqlMetadata {
    /// Connects on first use: a database that isn't up yet is waited for, not an error here.
    pub fn new(database_url: &str) -> Result<Self> {
        let client = MetadataClient::connect_lazy(&MetadataConfig {
            database_url: database_url.to_string(),
        })
        .context("the metadata database URL")?;
        Ok(Self { client })
    }
}

/// Whether a database error means "not up or not migrated yet": no connection, the server
/// starting (57P03), the database not created yet (3D000), a table or column the migrations
/// haven't made yet (42P01, 42703).
pub fn sql_not_ready(e: &sqlx::Error) -> Option<String> {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::PoolTimedOut => {
            Some(format!("the metadata database: {e}"))
        }
        sqlx::Error::Database(d)
            if matches!(
                d.code().as_deref(),
                Some("57P03" | "3D000" | "42P01" | "42703")
            ) =>
        {
            Some(format!("the metadata database isn't migrated yet: {d}"))
        }
        _ => None,
    }
}

/// The versions in `known` that `applied` lacks.
pub fn missing_migrations(known: &[i64], applied: &HashSet<i64>) -> Vec<i64> {
    known
        .iter()
        .copied()
        .filter(|v| !applied.contains(v))
        .collect()
}

impl Metadata for SqlMetadata {
    async fn migrated(&self) -> Result<Check<()>> {
        let applied: Vec<i64> =
            match sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success")
                .fetch_all(self.client.pool())
                .await
            {
                Ok(v) => v,
                Err(e) => match sql_not_ready(&e) {
                    Some(reason) => return Ok(Check::Wait(reason)),
                    None => return Err(e).context("reading the metadata migrations"),
                },
            };
        let known: Vec<i64> = MIGRATOR
            .iter()
            .filter(|m| m.migration_type.is_up_migration())
            .map(|m| m.version)
            .collect();
        let missing = missing_migrations(&known, &applied.into_iter().collect());
        Ok(match missing.first() {
            None => Check::Ready(()),
            Some(first) => Check::Wait(format!(
                "the orchestrator to migrate the metadata database ({} migrations left, from {first})",
                missing.len()
            )),
        })
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_migrations_are_named() {
        let applied: HashSet<i64> = [1, 2].into_iter().collect();
        assert_eq!(missing_migrations(&[1, 2, 3], &applied), [3]);
        assert!(missing_migrations(&[1, 2], &applied).is_empty());
    }

    // The bootstrap knows the metadata crate's migrations (compiled in from the same dir).
    #[test]
    fn the_migrator_knows_the_seed_migration() {
        let versions: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
        assert!(versions.contains(&20260927000002), "{versions:?}");
    }
}
