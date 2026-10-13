//! Binds the demo tenant to the local Zitadel org (`tenants.zitadel_org_id`).

use crate::paths::{compose_env, compose_file};
use anyhow::{Context, Result, bail};
use std::io::Write;
use std::path::Path;
use std::process::Command;
use tracing::{info, warn};

/// The metadata database's name: the path of `CHELABASE_METADATA_DATABASE_URL` when it has one
/// (a branch may run on its own metadata database), else `chelabase_metadata`.
fn metadata_db_name(url: Option<&str>) -> String {
    url.and_then(|u| u.trim().split_once("://"))
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| path.split(['?', '#']).next().unwrap_or("").to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "chelabase_metadata".to_string())
}

/// `docker` arguments that open a Compose call: `compose -p <project> -f <file> --env-file
/// <env>`. A workbench stack names its project (`CHELABASE_COMPOSE_PROJECT`); without one, main's
/// project (`chelabase`). The compose file has no `name:` of its own, so `-p` is always given.
pub fn compose_base(project: Option<&str>, file: &Path, env: &Path) -> Vec<String> {
    let project = project.map_or_else(
        || crate::workbench::slug::project_name(crate::workbench::slug::MAIN),
        str::to_string,
    );
    let mut args = vec!["compose".to_string(), "-p".to_string(), project];
    args.extend([
        "-f".to_string(),
        file.display().to_string(),
        "--env-file".to_string(),
        env.display().to_string(),
    ]);
    args
}

/// The current stack's Compose project, from `CHELABASE_COMPOSE_PROJECT` (unset or blank: none).
fn compose_project() -> Option<String> {
    std::env::var("CHELABASE_COMPOSE_PROJECT")
        .ok()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
}

/// `docker compose … exec -T metadata-db <cmd…>` in the current stack.
fn metadata_exec(cmd: &[&str]) -> Command {
    let mut command = Command::new("docker");
    command
        .args(compose_base(
            compose_project().as_deref(),
            &compose_file(),
            &compose_env(),
        ))
        .args(["exec", "-T", "metadata-db"])
        .args(cmd);
    command
}

/// `psql -U chelabase -d <database> <extra…>` in the current stack's metadata-db.
fn metadata_psql(database: &str, extra: &[&str]) -> Command {
    let mut command = metadata_exec(&["psql", "-U", "chelabase", "-d", database]);
    command.args(extra);
    command
}

fn metadata_database() -> String {
    metadata_db_name(
        std::env::var("CHELABASE_METADATA_DATABASE_URL")
            .ok()
            .as_deref(),
    )
}

pub fn bind_demo_tenant(org_id: &str, demo_tenant: &str) -> Result<()> {
    let database = metadata_database();

    let ready = metadata_exec(&["pg_isready", "-U", "chelabase", "-d", &database])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !ready {
        warn!("metadata Postgres not ready — skipped tenant bind (re-run once the stack is up: just workbench up)");
        return Ok(());
    }

    let col = metadata_psql(
        &database,
        &[
            "-tAc",
            "SELECT 1 FROM information_schema.columns WHERE table_name='tenants' AND column_name='zitadel_org_id'",
        ],
    )
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
    let status = metadata_psql(&database, &["-v", "ON_ERROR_STOP=1", "-c", &sql])
        .stdout(std::process::Stdio::null())
        .status()
        .context("bind demo tenant")?;

    if !status.success() {
        bail!("failed to UPDATE tenants.zitadel_org_id");
    }
    info!("bound demo tenant {demo_tenant} → zitadel_org_id={org_id} (in {database})");
    Ok(())
}

/// Makes a Zitadel user the tenant's active admin: the same writes as the metadata crate's
/// `bootstrap_admin`. Run by psql with `-1 -v tenant=… -v zuid=… -v email=…`, so values are
/// quoted by psql. A repeat writes nothing: the upsert only touches a row that differs, and the
/// `admin_set` event (no actor) is logged only when the status or the flag changed. An email
/// another live member holds is cleared first (R8, the newest claim wins), and an invited
/// member's open invites are marked accepted. The CLI keeps this SQL rather than calling the
/// metadata crate (which would bring sqlx and a direct database connection into the CLI, that
/// today only reaches the database through `docker compose exec psql`); the metadata crate's
/// live test `bootstrap_admin_sql_matches_the_metadata_crate` runs this file against
/// `bootstrap_admin` and compares what each writes.
pub(crate) const ADMIN_SQL: &str = include_str!("tenant_admin.sql");

/// Names the tenant after the Zitadel org when it has no display name yet (R36), with the same
/// `tenant_renamed` event (no actor) an admin's rename writes. Run by psql with
/// `-1 -v tenant=… -v name=…`. A tenant that has a name keeps it, so a repeat writes nothing.
/// The name is trimmed and cut to 80 characters, as the column requires.
pub(crate) const NAME_SQL: &str = include_str!("tenant_name.sql");

/// The tenant name for a Zitadel org name, as the app would store it (R36): control characters
/// stripped, trimmed, at most 80 characters; `None` for an empty name or Zitadel's default org
/// name ("ZITADEL"), which says nothing about the tenant.
pub fn tenant_name_from_org(org_name: &str) -> Option<String> {
    let cleaned: String = org_name.chars().filter(|c| !c.is_control()).collect();
    let name: String = cleaned.trim().chars().take(80).collect();
    let name = name.trim().to_string();
    (!name.is_empty() && name != "ZITADEL").then_some(name)
}

/// Runs [`NAME_SQL`]. Skips with a warning while the metadata database or the `display_name`
/// column isn't there yet.
pub fn fill_display_name(demo_tenant: &str, org_name: &str) -> Result<()> {
    let Some(org_name) = tenant_name_from_org(org_name) else {
        return Ok(());
    };
    let database = metadata_database();
    let psql = |extra: &[&str]| metadata_psql(&database, extra);
    let col = psql(&[
        "-tAc",
        "SELECT 1 FROM information_schema.columns WHERE table_name='tenants' AND column_name='display_name'",
    ])
    .output()
    .context("check display_name column")?;
    if String::from_utf8_lossy(&col.stdout).trim() != "1" {
        warn!("tenants.display_name missing — start orchestrator once (migrate), then re-run bootstrap");
        return Ok(());
    }
    let mut child = psql(&[
        "-1",
        "-v",
        "ON_ERROR_STOP=1",
        "-v",
        &format!("tenant={demo_tenant}"),
        "-v",
        &format!("name={org_name}"),
        "-f",
        "-",
    ])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::null())
    .spawn()
    .context("name the tenant")?;
    child
        .stdin
        .take()
        .context("psql stdin")?
        .write_all(NAME_SQL.as_bytes())
        .context("send name SQL")?;
    if !child.wait()?.success() {
        bail!("failed to name the tenant");
    }
    info!("tenant {demo_tenant}: named after the Zitadel org when it had no name");
    Ok(())
}

/// Upserts `zitadel_user_id` as the tenant's active admin (see [`ADMIN_SQL`]). Skips with a
/// warning while the metadata database or its members table isn't there yet.
pub fn bootstrap_admin(demo_tenant: &str, zitadel_user_id: &str, email: &str) -> Result<()> {
    let database = metadata_database();
    let psql = |extra: &[&str]| metadata_psql(&database, extra);

    let table = psql(&["-tAc", "SELECT to_regclass('public.members') IS NOT NULL"])
        .output()
        .context("check members table")?;
    if String::from_utf8_lossy(&table.stdout).trim() != "t" {
        warn!(
            "members table missing — start orchestrator once (migrate), then re-run bootstrap to set the tenant admin"
        );
        return Ok(());
    }

    let mut child = psql(&[
        "-1",
        "-v",
        "ON_ERROR_STOP=1",
        "-v",
        &format!("tenant={demo_tenant}"),
        "-v",
        &format!("zuid={zitadel_user_id}"),
        "-v",
        &format!("email={email}"),
        "-f",
        "-",
    ])
    .stdin(std::process::Stdio::piped())
    .stdout(std::process::Stdio::null())
    .spawn()
    .context("set tenant admin")?;
    child
        .stdin
        .take()
        .context("psql stdin")?
        .write_all(ADMIN_SQL.as_bytes())
        .context("send admin SQL")?;
    if !child.wait()?.success() {
        bail!("failed to set the tenant admin");
    }
    info!("tenant {demo_tenant}: {email} is an active admin (member {zitadel_user_id})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{compose_base, metadata_db_name, tenant_name_from_org, NAME_SQL};

    #[test]
    fn org_names_become_tenant_names_like_the_apps() {
        assert_eq!(tenant_name_from_org("  Acme Corp \n").as_deref(), Some("Acme Corp"));
        assert_eq!(tenant_name_from_org("A\u{7}c\tme").as_deref(), Some("Acme"));
        assert_eq!(tenant_name_from_org(&"é".repeat(90)).unwrap().chars().count(), 80);
        // Zitadel's own default name, and names with nothing left, say nothing.
        assert_eq!(tenant_name_from_org("ZITADEL"), None);
        assert_eq!(tenant_name_from_org(" \u{1}  "), None);
        assert_eq!(tenant_name_from_org(""), None);
    }

    #[test]
    fn name_sql_only_fills_an_empty_name_and_logs_one_event() {
        // Fills a NULL name only, so a name an admin chose is never overwritten.
        assert!(NAME_SQL.contains("display_name IS NULL"), "{NAME_SQL}");
        // The stored value satisfies the column's check: trimmed, at most 80 characters.
        assert!(NAME_SQL.contains("btrim(left(btrim(:'name'), 80))"), "{NAME_SQL}");
        // One event with no actor, only for the row that changed.
        assert!(NAME_SQL.contains("'tenant_renamed'") && NAME_SQL.contains("NULL::uuid"), "{NAME_SQL}");
        assert!(NAME_SQL.contains("FROM u"), "{NAME_SQL}");
        // Values go through psql variables, never string-built into the SQL.
        for var in [":'tenant'", ":'name'"] {
            assert!(NAME_SQL.contains(var), "{var} missing");
        }
    }

    #[test]
    fn metadata_db_name_follows_the_url() {
        assert_eq!(metadata_db_name(None), "chelabase_metadata");
        assert_eq!(metadata_db_name(Some("  ")), "chelabase_metadata");
        assert_eq!(
            metadata_db_name(Some("postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata_6b")),
            "chelabase_metadata_6b"
        );
        assert_eq!(
            metadata_db_name(Some("postgres://u:p@h:5433/other?sslmode=disable")),
            "other"
        );
        assert_eq!(metadata_db_name(Some("postgres://u:p@h:5433/")), "chelabase_metadata");
        assert_eq!(metadata_db_name(Some("not a url")), "chelabase_metadata");
    }

    #[test]
    fn tenant_bind_runs_in_the_stacks_project() {
        use std::path::Path;
        assert_eq!(
            compose_base(Some("chelabase-feat-x"), Path::new("/w/c.yml"), Path::new("/s/stack.env")),
            ["compose", "-p", "chelabase-feat-x", "-f", "/w/c.yml", "--env-file", "/s/stack.env"]
        );
        assert_eq!(
            compose_base(None, Path::new("/w/c.yml"), Path::new("/e")),
            ["compose", "-p", "chelabase", "-f", "/w/c.yml", "--env-file", "/e"],
            "outside a workbench, the main project by name: compose never runs without -p (T5 review M5)"
        );
    }
}
