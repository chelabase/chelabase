//! The machine keys the workbench's key phase ensures, as their tests compare against them. The
//! bootstrap itself is `chelabase-init bootstrap --demo` (`chelabase_init::bootstrap`), run
//! inside the stack by `workbench::commands::bootstrap`.

#[cfg(test)]
use crate::dev::{app_keys, internal_keys, secrets_key};
#[cfg(test)]
use crate::neon::{compute_ca, compute_jwt};
#[cfg(test)]
use anyhow::{Context, Result};
#[cfg(test)]
use std::path::Path;

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
