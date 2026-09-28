//! Binds the demo tenant to the local Zitadel org (`tenants.zitadel_org_id`).

use crate::paths::{compose_env, compose_file};
use anyhow::{Context, Result, bail};
use std::process::Command;
use tracing::{info, warn};

pub fn bind_demo_tenant(org_id: &str, demo_tenant: &str) -> Result<()> {
    let compose = compose_file();
    let env_file = compose_env();

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
            "cheladb_metadata",
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
            "cheladb_metadata",
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
            "cheladb_metadata",
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
    info!("bound demo tenant {demo_tenant} → zitadel_org_id={org_id}");
    Ok(())
}
