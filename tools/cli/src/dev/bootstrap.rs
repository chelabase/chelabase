//! `chelabase dev bootstrap`: idempotent local IAM setup on a fresh Zitadel v4 instance.

use crate::dev::env_files::{DemoEnv, render_demo_env, render_studio_env};
use crate::dev::zitadel::HttpZitadel;
use crate::dev::{app_keys, internal_keys, secrets_key, steps, tenant};
use crate::neon::compute_jwt;
use crate::paths::{
    auth_dir, demo_tenant_id, entry, gateway_dir, neon_dir, secrets_dir, studio_env_file,
    zitadel_dir,
};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::Path;
use std::time::Duration;
use tracing::{info, warn};

/// The email asked for (trimmed, lowercased, like the members table), if any: a requested email
/// that no Zitadel user has is an error, the default only a warning.
fn requested_admin_email(requested: Option<&str>) -> Option<String> {
    requested
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(chelabase_zitadel::rules::normalize_email)
}

/// Zitadel's first-instance human admin, the dev stack's sign-in, is named after the instance's
/// external domain: `zitadel-admin@zitadel.<auth host name>` on a fresh stack. An instance made
/// before W1 keeps `zitadel-admin@zitadel.localhost`. The derived email comes first.
fn default_admin_emails(auth_host_name: &str) -> Vec<String> {
    let mut emails = vec![
        format!("zitadel-admin@zitadel.{auth_host_name}"),
        "zitadel-admin@zitadel.localhost".to_string(),
    ];
    emails.dedup();
    emails
}

/// Whether the demo machine user is made a tenant admin: on a self-hosted dev stack only.
fn demo_admin(mode: chelabase_types::mode::Mode) -> bool {
    mode == chelabase_types::mode::Mode::SelfHosted
}

/// `dev bootstrap` writes into a workbench stack's state dir (`CHELABASE_STACK_DIR`, set by the
/// stack's env). Without it, it would recreate the retired zitadel dir under `.chelabase`.
pub fn check_stack_env(stack_dir: Option<&str>) -> Result<()> {
    if stack_dir.is_none_or(|d| d.trim().is_empty()) {
        bail!(
            "CHELABASE_STACK_DIR isn't set: `dev bootstrap` runs with a workbench stack's env. \
             Use `just workbench up` (it bootstraps), or `just zitadel-bootstrap`, which loads \
             the stack env"
        );
    }
    Ok(())
}

/// Set to `1` by the workbench's bootstrap before the apps: Zitadel refuses an Actions target
/// whose host doesn't resolve (`Errors.Target.DeniedURL`), and on a fresh stack the orchestrator
/// container doesn't exist yet. The bootstrap after the apps creates the target.
pub const SKIP_ACTIONS_VAR: &str = "CHELABASE_BOOTSTRAP_SKIP_ACTIONS";

pub fn skip_actions_target(v: Option<&str>) -> bool {
    v.is_some_and(|v| v.trim() == "1")
}

pub async fn run(admin_email: Option<&str>) -> Result<()> {
    check_stack_env(std::env::var("CHELABASE_STACK_DIR").ok().as_deref())?;
    let entry = entry()?;
    let iss = entry.issuer();
    let studio_origin = entry.studio_origin();
    let zdir = zitadel_dir();
    fs::create_dir_all(&zdir).with_context(|| format!("mkdir {}", zdir.display()))?;

    // Local files first, so they exist even if Zitadel isn't up.
    let cookie_dir = auth_dir().with_context(|| {
        format!(
            "migrate auth cookie dir from {} to {}",
            crate::paths::repo_root().join(".chelabase/auth").display(),
            crate::paths::repo_root().join(".chelabase/auth").display()
        )
    })?;
    let MachineKeys {
        internal,
        secrets,
        cookie_key,
        compute_jwt,
        app_jwt,
    } = ensure_machine_keys(&gateway_dir(), &secrets_dir(), &cookie_dir, &neon_dir())?;

    let admin_pat = wait_for_file(&zdir.join("admin.pat"), "admin.pat", 45, TOKEN_POLL).await?;
    let login_client = zdir.join("login-client.pat");
    wait_for_file(&login_client, "login-client.pat", 45, TOKEN_POLL).await?;

    let mode = chelabase_types::mode::Mode::from_env().map_err(anyhow::Error::msg)?;
    // Zitadel at its upstream with the auth host as Host: the bootstrap runs before the gateway.
    let upstream = chelabase_types::entry::zitadel_upstream(|name| std::env::var(name).ok());
    let api = HttpZitadel::via_upstream(&upstream, entry.auth_host(), &admin_pat);
    let (org_id, org_name) = wait_ready(|| steps::ensure_org(&api)).await?;
    let project_id = steps::ensure_project(&api, &org_id).await?;
    let api_client_id = steps::ensure_api_app(&api, &project_id).await?;
    let studio_client_id =
        steps::ensure_studio_app(&api, &project_id, mode, &studio_origin).await?;
    // W8: self-hosted mode only, sign-ins go to Studio's own login.
    steps::ensure_studio_login(&api, &project_id, mode, &studio_origin).await?;
    let demo_user_id = steps::ensure_demo_user(&api, &org_id).await?;
    steps::ensure_demo_key(&api, &demo_user_id, &zdir.join("demo-sa.json")).await?;
    steps::ensure_smtp(&api).await?;
    steps::ensure_zitadel_registration_off(&api).await?;
    if skip_actions_target(std::env::var(SKIP_ACTIONS_VAR).ok().as_deref()) {
        info!("Actions target left for the bootstrap after the apps ({SKIP_ACTIONS_VAR})");
    } else {
        let actions_endpoint = steps::actions_endpoint_from(
            std::env::var("CHELABASE_ZITADEL_ACTIONS_ENDPOINT")
                .ok()
                .as_deref(),
        );
        steps::ensure_actions_target(&api, &zdir.join("actions.env"), &actions_endpoint)
            .await?;
    }

    // The admin is looked up before the org is bound to the tenant: a tenant with no member
    // lets the first person to sign in claim the admin, so a bind must never be left without
    // its admin when the admin was asked for.
    let requested = requested_admin_email(admin_email);
    let candidates = match &requested {
        Some(email) => vec![email.clone()],
        None => default_admin_emails(&chelabase_types::entry::normalize_host(entry.auth_host())),
    };
    let mut admin = None;
    for candidate in &candidates {
        if let Some(user_id) = steps::find_user_by_email(&api, &org_id, candidate).await? {
            admin = Some((user_id, candidate.clone()));
            break;
        }
    }
    if admin.is_none() && requested.is_some() {
        bail!(
            "no Zitadel user with email {} in the org: create that user in Zitadel first",
            candidates[0]
        );
    }

    let demo_tenant = demo_tenant_id();
    tenant::bind_demo_tenant(&org_id, &demo_tenant)?;
    if let Some(name) = &org_name {
        tenant::fill_display_name(&demo_tenant, name)?;
    }
    match &admin {
        Some((user_id, email)) => tenant::bootstrap_admin(&demo_tenant, user_id, email)?,
        None => warn!(
            "dev user not found in Zitadel (tried {}): no tenant admin is set, so the first \
             person to sign in to Studio claims it. Re-run with --admin-email before exposing \
             Studio",
            candidates.join(", ")
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
        render_studio_env(
            &iss,
            &studio_client_id,
            &project_id,
            &org_id,
            &entry.api_base(),
            entry.studio_host(),
        ),
    )?;
    info!("bootstrap complete (idempotent)");
    Ok(())
}

/// The machine keys bootstrap ensures before it talks to Zitadel.
pub struct MachineKeys {
    pub internal: internal_keys::InternalKeyPaths,
    pub secrets: std::path::PathBuf,
    pub cookie_key: std::path::PathBuf,
    pub compute_jwt: compute_jwt::ComputeJwtPaths,
    pub app_jwt: app_keys::AppKeyPaths,
}

/// Creates the machine keys that are missing (never overwrites one): the gateway's internal key
/// pair, the secrets key, auth's cookie key and app-JWT pair, and the compute-JWT pair. The
/// workbench runs this before any container mounts the key dirs.
pub fn ensure_machine_keys(
    gateway: &Path,
    secrets: &Path,
    auth: &Path,
    neon: &Path,
) -> Result<MachineKeys> {
    Ok(MachineKeys {
        internal: internal_keys::ensure(gateway)?,
        secrets: secrets_key::ensure(secrets)?,
        cookie_key: secrets_key::ensure_file(auth, secrets_key::COOKIE_KEY_FILE)?,
        compute_jwt: compute_jwt::ensure(neon)?,
        app_jwt: app_keys::ensure(auth)?,
    })
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

    // W2 live: Zitadel names its first admin after the instance's external domain, so a fresh
    // stack's admin is `zitadel-admin@zitadel.<auth host name>`; an instance made before W1
    // (main's) keeps the old `zitadel.localhost` name. The default tries both, in that order.
    #[test]
    fn the_default_admin_follows_zitadels_external_domain() {
        assert_eq!(
            default_admin_emails("auth.feat-x.chelabase.localhost"),
            [
                "zitadel-admin@zitadel.auth.feat-x.chelabase.localhost",
                "zitadel-admin@zitadel.localhost"
            ]
        );
        assert_eq!(default_admin_emails("localhost"), ["zitadel-admin@zitadel.localhost"]);
    }

    #[test]
    fn admin_email_is_only_requested_when_given() {
        assert_eq!(requested_admin_email(None), None);
        assert_eq!(requested_admin_email(Some("  ")), None);
        // Emails are stored trimmed and lowercased; an explicit one is required to exist.
        assert_eq!(
            requested_admin_email(Some(" Ada@Example.COM ")),
            Some("ada@example.com".to_string())
        );
    }

    #[test]
    fn the_actions_target_is_skipped_only_when_asked() {
        assert!(skip_actions_target(Some("1")));
        assert!(skip_actions_target(Some(" 1\n")));
        assert!(!skip_actions_target(None));
        assert!(!skip_actions_target(Some("")));
        assert!(!skip_actions_target(Some("0")));
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
}
