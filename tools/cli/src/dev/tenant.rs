//! Binds the demo tenant to the local Zitadel org (`tenants.zitadel_org_id`).

use crate::paths::{compose_env, compose_file};
use anyhow::{Context, Result, bail};
use std::process::Command;
use tracing::{info, warn};

/// The metadata database's name: the path of `CHELADB_METADATA_DATABASE_URL` when it has one
/// (a branch may run on its own metadata database), else `cheladb_metadata`.
fn metadata_db_name(url: Option<&str>) -> String {
    url.and_then(|u| u.trim().split_once("://"))
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| path.split(['?', '#']).next().unwrap_or("").to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "cheladb_metadata".to_string())
}

pub fn bind_demo_tenant(org_id: &str, demo_tenant: &str) -> Result<()> {
    let compose = compose_file();
    let env_file = compose_env();
    let url = std::env::var("CHELADB_METADATA_DATABASE_URL").ok();
    let database = metadata_db_name(url.as_deref());

    let ready = Command::new("docker")
        .args([
            "compose",
            "-f",
            compose.to_str().unwrap(),
            "--env-file",
            env_file.to_str().unwrap(),
            "exec",
            "-T",
            "metadata-db",
            "pg_isready",
            "-U",
            "cheladb",
            "-d",
            &database,
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !ready {
        warn!("metadata Postgres not ready — skipped tenant bind (re-run after deps-up)");
        return Ok(());
    }

    let col = Command::new("docker")
        .args([
            "compose",
            "-f",
            compose.to_str().unwrap(),
            "--env-file",
            env_file.to_str().unwrap(),
            "exec",
            "-T",
            "metadata-db",
            "psql",
            "-U",
            "cheladb",
            "-d",
            &database,
            "-tAc",
            "SELECT 1 FROM information_schema.columns WHERE table_name='tenants' AND column_name='zitadel_org_id'",
        ])
        .output()
        .context("check zitadel_org_id column")?;

    let col_exists = String::from_utf8_lossy(&col.stdout).trim() == "1";
    if !col_exists {
        warn!(
            "tenants.zitadel_org_id missing — start orchestrator once (migrate), then re-run bootstrap"
        );
        return Ok(());
    }

    let sql = format!("UPDATE tenants SET zitadel_org_id = '{org_id}' WHERE id = '{demo_tenant}';");
    let status = Command::new("docker")
        .args([
            "compose",
            "-f",
            compose.to_str().unwrap(),
            "--env-file",
            env_file.to_str().unwrap(),
            "exec",
            "-T",
            "metadata-db",
            "psql",
            "-U",
            "cheladb",
            "-d",
            &database,
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            &sql,
        ])
        .stdout(std::process::Stdio::null())
        .status()
        .context("bind demo tenant")?;

    if !status.success() {
        bail!("failed to UPDATE tenants.zitadel_org_id");
    }
    info!("bound demo tenant {demo_tenant} → zitadel_org_id={org_id} (in {database})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::metadata_db_name;

    #[test]
    fn metadata_db_name_follows_the_url() {
        assert_eq!(metadata_db_name(None), "cheladb_metadata");
        assert_eq!(metadata_db_name(Some("  ")), "cheladb_metadata");
        assert_eq!(
            metadata_db_name(Some("postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata_6b")),
            "cheladb_metadata_6b"
        );
        assert_eq!(
            metadata_db_name(Some("postgres://u:p@h:5433/other?sslmode=disable")),
            "other"
        );
        assert_eq!(metadata_db_name(Some("postgres://u:p@h:5433/")), "cheladb_metadata");
        assert_eq!(metadata_db_name(Some("not a url")), "cheladb_metadata");
    }
}
