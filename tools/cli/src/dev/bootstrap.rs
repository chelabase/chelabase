//! `chelabase dev bootstrap`'s workbench side. The bootstrap itself is
//! `chelabase-init bootstrap --demo` (`chelabase_init::bootstrap`), run inside the stack by
//! `workbench::commands::bootstrap`; this module renders the env files the workbench's apps
//! still read (`demo.env`, `actions.env`, `studio.env`) from its output files.

use crate::dev::env_files::{
    DemoEnv, actions_key_from_env, render_actions_env, render_demo_env, render_studio_env,
};
use crate::dev::{app_keys, internal_keys, secrets_key};
#[cfg(test)]
use crate::neon::{compute_ca, compute_jwt};
use anyhow::{Context, Result};
use chelabase_init::bootstrap::outputs::{self, Outputs};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A stack whose `actions.env` an earlier bootstrap wrote keeps its Actions target: the key
/// is copied into the output dir's `actions-signing-key` (Zitadel shows it only once), so the
/// bootstrap doesn't replace the target.
pub fn adopt_actions_key(out_dir: &Path) -> Result<()> {
    let out = Outputs::open(out_dir)?;
    if out.read_value(outputs::ACTIONS_SIGNING_KEY)?.is_some() {
        return Ok(());
    }
    let Ok(text) = std::fs::read_to_string(out_dir.join("actions.env")) else {
        return Ok(());
    };
    if let Some(key) = actions_key_from_env(&text) {
        out.write_secret(outputs::ACTIONS_SIGNING_KEY, format!("{key}\n").as_bytes())?;
    }
    Ok(())
}

/// One of the stack env's dirs, resolved (it exists: the key phase made it).
fn env_dir(env: &HashMap<String, String>, key: &str) -> Result<PathBuf> {
    let dir = env
        .get(key)
        .with_context(|| format!("the stack env has no {key}"))?;
    Path::new(dir)
        .canonicalize()
        .with_context(|| format!("{key}: resolving {dir}"))
}

fn required(out: &Outputs, name: &str) -> Result<String> {
    out.read_value(name)?.with_context(|| {
        format!(
            "the bootstrap left no {}",
            out.path().join(name).display()
        )
    })
}

/// Writes `contents` to `path` (0600) when it differs: temporary file, then rename.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if std::fs::read(path).ok().as_deref() == Some(contents.as_bytes()) {
        return Ok(());
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        // O_EXCL: a symlink planted at the temporary name is refused, never followed.
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Renders `demo.env` and `actions.env` (in the output dir, the stack's `zitadel/`) and the
/// stack's `studio.env` from the bootstrap's output files and the stack env `env` (its hosts
/// and its key dirs, which the key phase made: nothing is created here).
pub fn render_workbench_files(
    out_dir: &Path,
    studio_env: &Path,
    env: &HashMap<String, String>,
) -> Result<()> {
    let out = Outputs::open(out_dir)?;
    let entry = chelabase_types::entry::Entry::from_lookup(|k| env.get(k).cloned())?;
    let iss = entry.issuer();
    let org = required(&out, outputs::ORG_ID)?;
    let project = required(&out, outputs::PROJECT_ID)?;
    let api_client = required(&out, outputs::API_CLIENT_ID)?;
    let studio_client = required(&out, outputs::STUDIO_CLIENT_ID)?;
    let demo_user = required(&out, outputs::DEMO_USER_ID)?;
    let actions_key = required(&out, outputs::ACTIONS_SIGNING_KEY)?;
    let gateway = env_dir(env, "CHELABASE_GATEWAY_DIR")?;
    let auth = env_dir(env, "CHELABASE_AUTH_DIR")?;
    let neon = env_dir(env, "CHELABASE_NEON_DIR")?;
    let secrets = env_dir(env, "CHELABASE_SECRETS_DIR")?;
    let zdir = out_dir
        .canonicalize()
        .with_context(|| format!("resolving {}", out_dir.display()))?;
    let path = |p: PathBuf| p.display().to_string();
    let demo = render_demo_env(&DemoEnv {
        issuer: &iss,
        audiences: &format!("{project},{api_client}"),
        org_id: &org,
        project_id: &project,
        api_client_id: &api_client,
        demo_user_id: &demo_user,
        demo_tenant: &chelabase_init::bootstrap::SEEDED_TENANT.to_string(),
        signing_key: &path(gateway.join(internal_keys::SIGNING_KEY_FILE)),
        verify_key: &path(gateway.join(internal_keys::VERIFY_KEY_FILE)),
        secrets_key: &path(secrets.join(secrets_key::SECRETS_KEY_FILE)),
        auth_token_file: &path(zdir.join("login-client.pat")),
        auth_cookie_key: &path(auth.join(secrets_key::COOKIE_KEY_FILE)),
        compute_jwt_dir: &path(neon),
        admin_pat_file: &path(zdir.join("admin.pat")),
        app_jwt_private_key: &path(auth.join(app_keys::PRIVATE_FILE)),
        app_jwt_public_key: &path(auth.join(app_keys::PUBLIC_FILE)),
    });
    write_private(&zdir.join("demo.env"), &demo)?;
    write_private(&zdir.join("actions.env"), &render_actions_env(&actions_key))?;
    let studio = render_studio_env(
        &iss,
        &studio_client,
        &project,
        &org,
        &entry.api_base(),
        entry.studio_host(),
    );
    if std::fs::read_to_string(studio_env).ok().as_deref() != Some(studio.as_str()) {
        std::fs::write(studio_env, studio)
            .with_context(|| format!("writing {}", studio_env.display()))?;
    }
    Ok(())
}

/// The machine keys the old bootstrap ensured (the key phase's tests compare against them).
#[cfg(test)]
#[allow(dead_code, reason = "its tests read only the client pairs")]
pub struct MachineKeys {
    pub internal: internal_keys::InternalKeyPaths,
    pub secrets: std::path::PathBuf,
    pub cookie_key: std::path::PathBuf,
    pub compute_jwt: compute_jwt::ComputeJwtPaths,
    pub app_jwt: app_keys::AppKeyPaths,
    pub compute_ca: compute_ca::ComputeCaPaths,
    /// The directory holding the orchestrator's client pair (`current/` inside it).
    pub orchestrator_tls: std::path::PathBuf,
    /// The directory holding realtime's client pair, CN `chelabase-realtime`.
    pub realtime_tls: std::path::PathBuf,
}

/// Creates the machine keys that are missing (never overwrites one): the gateway's internal key
/// pair, the secrets key, auth's cookie key and app-JWT pair, the compute-JWT pair, the compute
/// CA and the orchestrator's and realtime's first client pairs (renewed by the worker's sweep). The
/// workbench runs this before any container mounts the key dirs.
#[cfg(test)]
pub fn ensure_machine_keys(
    gateway: &Path,
    secrets: &Path,
    auth: &Path,
    neon: &Path,
    compute_ca_dir: &Path,
    orchestrator_tls: &Path,
    realtime_tls: &Path,
) -> Result<MachineKeys> {
    let compute_ca = compute_ca::ensure(compute_ca_dir)?;
    std::fs::create_dir_all(orchestrator_tls)
        .with_context(|| format!("mkdir {}", orchestrator_tls.display()))?;
    let orchestrator_tls = orchestrator_tls
        .canonicalize()
        .with_context(|| format!("resolve {}", orchestrator_tls.display()))?;
    compute_ca::ensure_client_pair(
        &compute_ca.dir,
        &orchestrator_tls,
        std::time::SystemTime::now(),
    )?;
    std::fs::create_dir_all(realtime_tls)
        .with_context(|| format!("mkdir {}", realtime_tls.display()))?;
    let realtime_tls = realtime_tls
        .canonicalize()
        .with_context(|| format!("resolve {}", realtime_tls.display()))?;
    chelabase_neon::compute_tls::renew_client_pair(
        &realtime_tls,
        &chelabase_neon::compute_tls::ComputeCa::load(&compute_ca.dir)?,
        chelabase_neon::compute_tls::REALTIME_CN,
        std::time::SystemTime::now(),
    )?;
    Ok(MachineKeys {
        compute_ca,
        orchestrator_tls,
        realtime_tls,
        internal: internal_keys::ensure(gateway)?,
        secrets: secrets_key::ensure(secrets)?,
        cookie_key: secrets_key::ensure_file(auth, secrets_key::COOKIE_KEY_FILE)?,
        compute_jwt: compute_jwt::ensure(neon)?,
        app_jwt: app_keys::ensure(auth)?,
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn private_dir(p: &Path) {
        std::fs::create_dir_all(p).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    // The workbench's env files say what today's bootstrap wrote, from the output files alone.
    #[test]
    fn the_workbench_env_files_come_from_the_bootstrap_outputs() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().canonicalize().unwrap();
        let zdir = root.join("zitadel");
        private_dir(&zdir);
        let mut env = HashMap::new();
        for (k, d) in [
            ("CHELABASE_GATEWAY_DIR", "gateway"),
            ("CHELABASE_AUTH_DIR", "auth"),
            ("CHELABASE_NEON_DIR", "neon"),
            ("CHELABASE_SECRETS_DIR", "secrets"),
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            env.insert(k.to_string(), root.join(d).display().to_string());
        }
        for (k, v) in [
            ("CHELABASE_PUBLIC_SCHEME", "http"),
            ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
            ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:8000"),
            ("CHELABASE_API_HOST", "api.chelabase.localhost:8000"),
        ] {
            env.insert(k.to_string(), v.to_string());
        }
        // An older stack's actions.env keeps its key.
        std::fs::write(zdir.join("actions.env"), "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=old\n").unwrap();
        adopt_actions_key(&zdir).unwrap();
        let out = Outputs::open(&zdir).unwrap();
        assert_eq!(out.read_value(outputs::ACTIONS_SIGNING_KEY).unwrap().as_deref(), Some("old"));
        for (n, v) in [
            (outputs::ORG_ID, "o1"),
            (outputs::PROJECT_ID, "p1"),
            (outputs::API_CLIENT_ID, "c-api"),
            (outputs::STUDIO_CLIENT_ID, "c-spa"),
            (outputs::DEMO_USER_ID, "u1"),
        ] {
            out.write_value(n, v).unwrap();
        }
        let studio = root.join("studio.env");
        render_workbench_files(&zdir, &studio, &env).unwrap();
        let demo = std::fs::read_to_string(zdir.join("demo.env")).unwrap();
        for line in [
            "CHELABASE_ZITADEL_ISSUER=http://auth.chelabase.localhost:8000\n",
            "CHELABASE_ZITADEL_AUDIENCES=p1,c-api\n",
            "CHELABASE_ZITADEL_ORG_ID=o1\n",
            "CHELABASE_DEMO_USER_ID=u1\n",
            "CHELABASE_DEMO_TENANT_ID=00000000-0000-4000-8000-000000000001\n",
        ] {
            assert!(demo.contains(line), "{line} in {demo}");
        }
        assert!(demo.contains(&format!("CHELABASE_INTERNAL_SIGNING_KEY={}/gateway/internal-signing.pem\n", root.display())));
        assert!(demo.contains(&format!("CHELABASE_ZITADEL_ADMIN_PAT_FILE={}/admin.pat\n", zdir.display())));
        assert_eq!(
            std::fs::read_to_string(zdir.join("actions.env")).unwrap(),
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=old\n"
        );
        let mode = std::fs::metadata(zdir.join("demo.env")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(std::fs::read_to_string(&studio).unwrap().contains("VITE_ZITADEL_CLIENT_ID=c-spa\n"));
        // Nothing was created in the key dirs.
        for d in ["gateway", "auth", "neon", "secrets"] {
            assert_eq!(std::fs::read_dir(root.join(d)).unwrap().count(), 0, "{d}");
        }
    }
}
