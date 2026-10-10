//! `chelabase dev bootstrap`: idempotent local IAM setup on a fresh Zitadel v4 instance.

use crate::dev::env_files::{DemoEnv, render_demo_env, render_studio_env};
use crate::dev::zitadel::HttpZitadel;
use crate::dev::{app_keys, internal_keys, secrets_key, steps, tenant};
use crate::neon::compute_jwt;
use crate::paths::{
    auth_dir, demo_tenant_id, gateway_dir, issuer, neon_dir, secrets_dir, studio_env_file,
    zitadel_dir,
};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

const GATEWAY_URL: &str = "http://localhost:8000";

/// Zitadel's first-instance human admin, the dev stack's sign-in.
pub const DEV_USER_EMAIL: &str = "zitadel-admin@zitadel.localhost";

/// The admin's email (trimmed, lowercased, like the members table) and whether it was asked
/// for: a requested email that no Zitadel user has is an error, the default only a warning.
fn resolve_admin_email(requested: Option<&str>) -> (String, bool) {
    match requested.map(str::trim).filter(|e| !e.is_empty()) {
        Some(email) => (chelabase_zitadel::rules::normalize_email(email), true),
        None => (DEV_USER_EMAIL.to_string(), false),
    }
}

/// Whether the demo machine user is made a tenant admin: on a self-hosted dev stack only.
fn demo_admin(mode: chelabase_types::mode::Mode) -> bool {
    mode == chelabase_types::mode::Mode::SelfHosted
}

pub async fn run(admin_email: Option<&str>) -> Result<()> {
    let iss = issuer();
    let zdir = zitadel_dir();
    fs::create_dir_all(&zdir).with_context(|| format!("mkdir {}", zdir.display()))?;

    // Local files first, so they exist even if Zitadel isn't up.
    let internal = internal_keys::ensure(&gateway_dir())?;
    let secrets = secrets_key::ensure(&secrets_dir())?;
    let cookie_dir = auth_dir().with_context(|| {
        format!(
            "migrate auth cookie dir from {} to {}",
            crate::paths::repo_root().join(".chelabase/auth").display(),
            crate::paths::repo_root().join(".chelabase/auth").display()
        )
    })?;
    let cookie_key = secrets_key::ensure_file(&cookie_dir, "cookie.key")?;
    let compute_jwt = compute_jwt::ensure(&neon_dir())?;
    let app_jwt = app_keys::ensure(&cookie_dir)?;

    let admin_pat = wait_for_file(&zdir.join("admin.pat"), "admin.pat", 45, TOKEN_POLL).await?;
    let login_client = zdir.join("login-client.pat");
    wait_for_file(&login_client, "login-client.pat", 45, TOKEN_POLL).await?;

    let mode = chelabase_types::mode::Mode::from_env().map_err(anyhow::Error::msg)?;
    let api = HttpZitadel::new(&iss, &admin_pat);
    let (org_id, org_name) = wait_ready(|| steps::ensure_org(&api)).await?;
    let project_id = steps::ensure_project(&api, &org_id).await?;
    let api_client_id = steps::ensure_api_app(&api, &project_id).await?;
    let studio_client_id = steps::ensure_studio_app(&api, &project_id, mode).await?;
    // W8: self-hosted mode only, sign-ins go to Studio's own login.
    steps::ensure_studio_login(&api, &project_id, mode).await?;
    let demo_user_id = steps::ensure_demo_user(&api, &org_id).await?;
    steps::ensure_demo_key(&api, &demo_user_id, &zdir.join("demo-sa.json")).await?;
    steps::ensure_smtp(&api).await?;
    steps::ensure_zitadel_registration_off(&api).await?;
    steps::ensure_actions_target(&api, &zdir.join("actions.env")).await?;

    // The admin is looked up before the org is bound to the tenant: a tenant with no member
    // lets the first person to sign in claim the admin, so a bind must never be left without
    // its admin when the admin was asked for.
    let (email, requested) = resolve_admin_email(admin_email);
    let admin = steps::find_user_by_email(&api, &org_id, &email).await?;
    if admin.is_none() && requested {
        bail!("no Zitadel user with email {email} in the org: create that user in Zitadel first");
    }

    let demo_tenant = demo_tenant_id();
    tenant::bind_demo_tenant(&org_id, &demo_tenant)?;
    if let Some(name) = &org_name {
        tenant::fill_display_name(&demo_tenant, name)?;
    }
    match &admin {
        Some(user_id) => tenant::bootstrap_admin(&demo_tenant, user_id, &email)?,
        None => warn!(
            "dev user {email} not found in Zitadel: no tenant admin is set, so the first person to \
             sign in to Studio claims it. Re-run with --admin-email before exposing Studio"
        ),
    }
    // Every `dev token` script acts as the demo machine user, so it is an admin too (self-hosted
    // dev stacks only; it has no email).
    if demo_admin(mode) {
        tenant::bootstrap_admin(&demo_tenant, &demo_user_id, "")?;
    }

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
            admin_pat_file: &path(&zdir.join("admin.pat").canonicalize()?),
            app_jwt_private_key: &path(&app_jwt.private),
            app_jwt_public_key: &path(&app_jwt.public),
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
async fn wait_ready<T, F, Fut>(call: F) -> Result<T>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
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

    #[test]
    fn admin_email_defaults_to_dev_user() {
        assert_eq!(resolve_admin_email(None), (DEV_USER_EMAIL.to_string(), false));
        assert_eq!(resolve_admin_email(Some("  ")), (DEV_USER_EMAIL.to_string(), false));
        // Emails are stored trimmed and lowercased; an explicit one is required to exist.
        assert_eq!(
            resolve_admin_email(Some(" Ada@Example.COM ")),
            ("ada@example.com".to_string(), true)
        );
    }

    #[test]
    fn the_demo_machine_user_is_an_admin_on_self_hosted_stacks_only() {
        use chelabase_types::mode::Mode;
        assert!(demo_admin(Mode::SelfHosted));
        assert!(!demo_admin(Mode::Platform));
    }

    #[test]
    fn admin_sql_stores_no_email_for_a_user_without_one() {
        // The demo machine user has none: NULL, not an empty string another such user would clash with.
        let sql = tenant::ADMIN_SQL;
        assert!(!sql.contains("lower(btrim(:'email'))\n") || sql.contains("NULLIF(lower(btrim(:'email')), '')"), "{sql}");
        for line in sql.lines().filter(|l| l.contains(":'email'")) {
            assert!(line.contains("NULLIF(lower(btrim(:'email')), '')"), "{line}");
        }
    }

    // M5: the module rule (`lock_tenant` in the metadata crate): a tenant is locked
    // FOR NO KEY UPDATE, which a concurrent insert of a member (a foreign key to it) doesn't wait
    // on. members_live `bootstrap_admin_sql_matches_the_metadata_crate` runs the SQL itself.
    #[test]
    fn admin_sql_locks_the_tenant_for_no_key_update() {
        let sql = tenant::ADMIN_SQL;
        assert!(sql.contains("FROM tenants WHERE id = :'tenant' FOR NO KEY UPDATE;"), "{sql}");
        assert!(!sql.contains("FOR UPDATE"), "{sql}");
    }

    #[test]
    fn admin_sql_is_idempotent_and_logs_event() {
        let sql = tenant::ADMIN_SQL;
        // Upsert keyed on the member's identity, writing only when something changes.
        assert!(sql.contains("INSERT INTO members"), "{sql}");
        assert!(sql.contains("ON CONFLICT (tenant_id, zitadel_user_id) DO UPDATE"), "{sql}");
        assert!(sql.contains("IS DISTINCT FROM"), "{sql}");
        assert!(sql.contains("tenant_admin = true"), "{sql}");
        assert!(sql.contains("status = 'active'"), "{sql}");
        // An email held by another live member is cleared first (R8).
        assert!(sql.contains("SET email = NULL"), "{sql}");
        assert!(sql.contains("status <> 'removed'"), "{sql}");
        // One admin_set event with no actor, only when the status or the flag changed.
        assert!(sql.contains("INSERT INTO access_events"), "{sql}");
        assert!(sql.contains("'admin_set'"), "{sql}");
        assert!(sql.contains("NULL::uuid, 'admin_set'"), "{sql}");
        assert!(sql.contains("p.id IS NULL OR p.status <> 'active' OR NOT p.tenant_admin"), "{sql}");
        // Values go through psql variables, never string-built into the SQL.
        for var in [":'tenant'", ":'zuid'", ":'email'"] {
            assert!(sql.contains(var), "{var} missing");
        }
    }

    // auth's SameSite=Strict cookies are dropped on cross-site fetches: Studio and the
    // gateway must be the same site (localhost vs 127.0.0.1 are different sites).
    #[test]
    fn studio_and_the_gateway_are_the_same_site() {
        assert_eq!(host(STUDIO_ORIGIN), host(GATEWAY_URL));
    }
}
