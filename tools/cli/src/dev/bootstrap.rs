//! `cheladb dev bootstrap`: idempotent local IAM setup on a fresh Zitadel v4 instance.

use crate::dev::env_files::{DemoEnv, render_demo_env, render_studio_env};
use crate::dev::zitadel::HttpZitadel;
use crate::dev::{internal_keys, secrets_key, steps, tenant};
use crate::neon::compute_jwt;
use crate::paths::{
    auth_dir, demo_tenant_id, gateway_dir, issuer, neon_dir, secrets_dir, studio_env_file,
    zitadel_dir,
};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tracing::info;

const GATEWAY_URL: &str = "http://localhost:8000";

pub async fn run() -> Result<()> {
    let iss = issuer();
    let zdir = zitadel_dir();
    fs::create_dir_all(&zdir).with_context(|| format!("mkdir {}", zdir.display()))?;

    // Local files first, so they exist even if Zitadel isn't up.
    let internal = internal_keys::ensure(&gateway_dir())?;
    let secrets = secrets_key::ensure(&secrets_dir())?;
    let cookie_dir = auth_dir().with_context(|| {
        format!(
            "migrate auth cookie dir from {} to {}",
            crate::paths::repo_root().join(".cheladb/auth").display(),
            crate::paths::repo_root().join(".cheladb/auth").display()
        )
    })?;
    let cookie_key = secrets_key::ensure_file(&cookie_dir, "cookie.key")?;
    let compute_jwt = compute_jwt::ensure(&neon_dir())?;

    let admin_pat = wait_for_file(&zdir.join("admin.pat"), "admin.pat", 45, TOKEN_POLL).await?;
    let login_client = zdir.join("login-client.pat");
    wait_for_file(&login_client, "login-client.pat", 45, TOKEN_POLL).await?;

    let mode = cheladb_types::mode::Mode::from_env().map_err(anyhow::Error::msg)?;
    let api = HttpZitadel::new(&iss, &admin_pat);
    let org_id = wait_ready(|| steps::ensure_org(&api)).await?;
    let project_id = steps::ensure_project(&api, &org_id).await?;
    let api_client_id = steps::ensure_api_app(&api, &project_id).await?;
    let studio_client_id = steps::ensure_studio_app(&api, &project_id, mode).await?;
    // W8: self-hosted mode only, sign-ins go to Studio's own login.
    steps::ensure_studio_login(&api, &project_id, mode).await?;
    let demo_user_id = steps::ensure_demo_user(&api, &org_id).await?;
    steps::ensure_demo_key(&api, &demo_user_id, &zdir.join("demo-sa.json")).await?;
    steps::ensure_smtp(&api).await?;

    let demo_tenant = demo_tenant_id();
    tenant::bind_demo_tenant(&org_id, &demo_tenant)?;

    let path = |p: &Path| p.display().to_string();
    let demo_env = zdir.join("demo.env");
    fs::write(
        &demo_env,
        render_demo_env(&DemoEnv {
            issuer: &iss,
            audiences: &format!("{project_id},{api_client_id}"),
            org_id: &org_id,
            project_id: &project_id,
            api_client_id: &api_client_id,
            demo_user_id: &demo_user_id,
            demo_tenant: &demo_tenant,
            signing_key: &path(&internal.signing),
            verify_key: &path(&internal.verify),
            secrets_key: &path(&secrets),
            auth_token_file: &path(&login_client.canonicalize()?),
            auth_cookie_key: &path(&cookie_key),
            compute_jwt_dir: &path(&compute_jwt.dir),
        }),
    )
    .with_context(|| format!("write {}", demo_env.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&demo_env, fs::Permissions::from_mode(0o600))?;
    }
    fs::write(
        studio_env_file(),
        render_studio_env(&iss, &studio_client_id, &project_id, &org_id, GATEWAY_URL),
    )?;
    info!("bootstrap complete (idempotent)");
    Ok(())
}

const TOKEN_POLL: Duration = Duration::from_secs(2);

async fn wait_for_file(path: &Path, what: &str, attempts: u32, delay: Duration) -> Result<String> {
    for _ in 0..attempts {
        if let Ok(text) = fs::read_to_string(path) {
            let token: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            if !token.is_empty() {
                return Ok(token);
            }
        }
        tokio::time::sleep(delay).await;
    }
    bail!(
        "missing {} ({what}): written only on a fresh Zitadel volume. Run: just zitadel-reset",
        path.display()
    )
}

/// Zitadel answers JWKS before its APIs are ready; retry the first real call.
async fn wait_ready<F, Fut>(call: F) -> Result<String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    let mut last = None;
    for _ in 0..60 {
        match call().await {
            Ok(value) => return Ok(value),
            Err(err) => last = Some(err),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(last.unwrap()).context("Zitadel API not ready")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::requests::STUDIO_ORIGIN;

    fn host(url: &str) -> &str {
        let rest = url.split_once("://").unwrap().1;
        rest.split(['/', ':']).next().unwrap()
    }

    // Zitadel writes admin.pat and login-client.pat moments apart on a fresh instance:
    // both are waited for, not just the first.
    #[tokio::test]
    async fn waits_for_a_token_file_then_reads_it_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("login-client.pat");
        let writer = {
            let path = path.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                std::fs::write(path, " tok\n").unwrap();
            })
        };
        let token = wait_for_file(&path, "login-client.pat", 50, Duration::from_millis(10)).await;
        writer.await.unwrap();
        assert_eq!(token.unwrap(), "tok");
    }

    #[tokio::test]
    async fn a_token_file_that_never_appears_says_to_reset() {
        let dir = tempfile::tempdir().unwrap();
        let err = wait_for_file(
            &dir.path().join("x.pat"),
            "x.pat",
            2,
            Duration::from_millis(1),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("just zitadel-reset"), "{err}");
    }

    // auth's SameSite=Strict cookies are dropped on cross-site fetches: Studio and the
    // gateway must be the same site (localhost vs 127.0.0.1 are different sites).
    #[test]
    fn studio_and_the_gateway_are_the_same_site() {
        assert_eq!(host(STUDIO_ORIGIN), host(GATEWAY_URL));
    }
}
