//! The seeded tenant's side of the bootstrap: bound to the Zitadel org, named after it, and
//! (`--demo` only) given its admins. Through `chelabase_metadata`'s client over the metadata
//! database URL: no `docker`, no `psql`.

use anyhow::{Context, Result, bail};
use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_types::TenantId;
use std::collections::HashSet;
use std::future::Future;
use uuid::Uuid;

use super::retry::Check;

/// What binding the tenant to `org` does, given the org it is bound to now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    /// Unbound: bind it.
    Set,
    /// Already bound to this org: nothing to do.
    Keep,
    /// Bound to another org: never rebound (a Zitadel volume lost while metadata was kept
    /// would otherwise hand the tenant to a fresh org).
    Refuse { bound: String },
}

pub fn bind_decision(current: Option<&str>, org: &str) -> Bind {
    match current {
        None => Bind::Set,
        Some(c) if c == org => Bind::Keep,
        Some(c) => Bind::Refuse {
            bound: c.to_string(),
        },
    }
}

/// The refusal's message, naming both orgs.
pub fn refused(tenant: Uuid, bound: &str, org: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "tenant {tenant} is bound to Zitadel org {bound}, but this Zitadel's org is {org}: \
         refusing to rebind it (was Zitadel's volume replaced? restore it, or reset the \
         metadata database too)"
    )
}

/// The tenant name for a Zitadel org name, as the app would store it (R36): control characters
/// stripped, trimmed, at most 80 characters; `None` for an empty name or Zitadel's default org
/// name ("ZITADEL"), which says nothing about the tenant.
pub fn tenant_name_from_org(org_name: &str) -> Option<String> {
    let cleaned: String = org_name.chars().filter(|c| !c.is_control()).collect();
    let name: String = cleaned.trim().chars().take(80).collect();
    let name = name.trim().to_string();
    (!name.is_empty() && name != "ZITADEL").then_some(name)
}

pub trait Metadata: Send + Sync {
    /// Ready once the database answers and every migration this build knows is applied (the
    /// orchestrator applies them).
    fn migrated(&self) -> impl Future<Output = Result<Check<()>>> + Send;
    /// Sets `tenants.zitadel_org_id` only when it is NULL or already `org` (see
    /// [`bind_decision`]); a different org fails, naming both, and writes nothing. Returns
    /// whether it wrote.
    fn bind_org(&self, tenant: Uuid, org: &str) -> impl Future<Output = Result<bool>> + Send;
    /// Names the tenant `name` when it has no name yet (one `tenant_renamed` event, no actor).
    fn fill_display_name(
        &self,
        tenant: Uuid,
        name: &str,
    ) -> impl Future<Output = Result<bool>> + Send;
    /// Makes the Zitadel user an active tenant admin (`MetadataClient::bootstrap_admin`); no
    /// email is stored as NULL.
    fn bootstrap_admin(
        &self,
        tenant: Uuid,
        zitadel_user_id: &str,
        email: Option<&str>,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// The metadata migrations this build knows (the orchestrator applies the same set).
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../chelabase-metadata/migrations");

const NAME_SQL: &str = include_str!("tenant_name.sql");

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

    async fn bind_org(&self, tenant: Uuid, org: &str) -> Result<bool> {
        let mut tx = self.client.pool().begin().await?;
        // Locked as `lock_tenant` does: a concurrent claim or member insert doesn't wait on it,
        // and two bootstraps can't both see NULL.
        let current: Option<Option<String>> = sqlx::query_scalar(
            "SELECT zitadel_org_id FROM tenants WHERE id = $1 FOR NO KEY UPDATE",
        )
        .bind(tenant)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(current) = current else {
            bail!(
                "tenant {tenant} doesn't exist in the metadata database (the seed migration makes it)"
            );
        };
        match bind_decision(current.as_deref(), org) {
            Bind::Keep => Ok(false),
            Bind::Refuse { bound } => Err(refused(tenant, &bound, org)),
            Bind::Set => {
                sqlx::query(
                    "UPDATE tenants SET zitadel_org_id = $2 WHERE id = $1 AND zitadel_org_id IS NULL",
                )
                .bind(tenant)
                .bind(org)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(true)
            }
        }
    }

    async fn fill_display_name(&self, tenant: Uuid, name: &str) -> Result<bool> {
        let done = sqlx::query(NAME_SQL)
            .bind(tenant)
            .bind(name)
            .execute(self.client.pool())
            .await?;
        Ok(done.rows_affected() > 0)
    }

    async fn bootstrap_admin(
        &self,
        tenant: Uuid,
        zitadel_user_id: &str,
        email: Option<&str>,
    ) -> Result<()> {
        self.client
            .bootstrap_admin(TenantId(tenant), zitadel_user_id, email.unwrap_or(""))
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_names_become_tenant_names_like_the_apps() {
        assert_eq!(
            tenant_name_from_org("  Acme Corp \n").as_deref(),
            Some("Acme Corp")
        );
        assert_eq!(tenant_name_from_org("A\u{7}c\tme").as_deref(), Some("Acme"));
        assert_eq!(
            tenant_name_from_org(&"é".repeat(90))
                .unwrap()
                .chars()
                .count(),
            80
        );
        // Zitadel's own default name, and names with nothing left, say nothing.
        assert_eq!(tenant_name_from_org("ZITADEL"), None);
        assert_eq!(tenant_name_from_org(" \u{1}  "), None);
        assert_eq!(tenant_name_from_org(""), None);
    }

    #[test]
    fn name_sql_only_fills_an_empty_name_and_logs_one_event() {
        // Fills a NULL name only, so a name an admin chose is never overwritten.
        assert!(NAME_SQL.contains("display_name IS NULL"), "{NAME_SQL}");
        assert!(
            NAME_SQL.contains("btrim(left(btrim($2), 80))"),
            "{NAME_SQL}"
        );
        assert!(
            NAME_SQL.contains("'tenant_renamed'") && NAME_SQL.contains("NULL::uuid"),
            "{NAME_SQL}"
        );
        assert!(NAME_SQL.contains("FROM u"), "{NAME_SQL}");
    }

    // The bind rule, exhaustively over the three cases: NULL binds, equal keeps, other refuses.
    #[test]
    fn the_tenant_is_bound_only_when_unbound_or_already_this_org() {
        assert_eq!(bind_decision(None, "B"), Bind::Set);
        assert_eq!(bind_decision(Some("B"), "B"), Bind::Keep);
        assert_eq!(
            bind_decision(Some("A"), "B"),
            Bind::Refuse { bound: "A".into() }
        );
        let msg = refused(Uuid::nil(), "A-org", "B-org").to_string();
        assert!(msg.contains("A-org") && msg.contains("B-org"), "{msg}");
    }

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
