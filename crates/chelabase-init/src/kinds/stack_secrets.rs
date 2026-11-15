//! The stack's generated service secrets (RT1 T1: R11, S1, S27): NATS and Valkey user passwords,
//! realtime's secrets and every internal bearer.
//!
//! Each lives in its own 0600 file under the keys dir (`<main>/.chelabase`), shared by every
//! stack of the machine, and is mounted only into the services that read it. A file is created
//! once and never rewritten: a running service (or a computed hash) may hold the old value. An
//! existing empty file is an error, never silently replaced.
//!
//! `chelabase_init::ensure_secrets` makes these files now ([`crate::secrets::LAYOUT`]). The
//! path-based [`ensure`] and [`SECRET_FILES`] stay only as the baseline the tests compare with
//! (feature `test-support`); `the_legacy_generators_match_the_layout` keeps them in step.

use anyhow::{Context, Result};
use std::path::Path;

#[cfg(any(test, feature = "test-support"))]
/// What a secret file holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// 32 random bytes as 43 base64url characters (`[A-Za-z0-9_-]`, safe in URLs and env files).
    Text,
    /// 32 random bytes, raw (an AES-256 key).
    RawKey,
}

#[cfg(any(test, feature = "test-support"))]
/// Every secret file, relative to the keys dir.
pub const SECRET_FILES: &[(&str, Kind)] = &[
    ("nats/orchestrator.password", Kind::Text),
    ("nats/worker.password", Kind::Text),
    ("nats/realtime.password", Kind::Text),
    ("nats/runner.password", Kind::Text),
    ("nats/storage.password", Kind::Text),
    ("nats/functions.password", Kind::Text),
    ("valkey/proxy.password", Kind::Text),
    ("valkey/control.password", Kind::Text),
    ("valkey/realtime.password", Kind::Text),
    ("valkey/runner.password", Kind::Text),
    // Ruling (f): Valkey's healthcheck logs in as a PING-only user.
    ("valkey/health.password", Kind::Text),
    ("realtime/pg.password", Kind::Text),
    ("realtime/ticket.key", Kind::RawKey),
    ("realtime/internal.token", Kind::Text),
    ("tokens/rest-internal.token", Kind::Text),
    ("tokens/data-keys.token", Kind::Text),
    ("tokens/app-auth.token", Kind::Text),
    ("tokens/app-sessions.token", Kind::Text),
    ("tokens/storage-internal.token", Kind::Text),
    ("tokens/orchestrator-functions.token", Kind::Text),
    ("tokens/worker-functions.token", Kind::Text),
    ("tokens/functions-service-runner.token", Kind::Text),
    ("tokens/worker-service-runner.token", Kind::Text),
    ("tokens/proxy-api.token", Kind::Text),
    ("tokens/storcon-hook.token", Kind::Text),
];

/// The NATS users, one per service plus the test runner (`storage`: object storage m9;
/// `functions`: edge functions m4b).
pub const NATS_USERS: [&str; 6] = ["orchestrator", "worker", "realtime", "runner", "storage", "functions"];
/// The Valkey ACL users: the proxy, the control plane (orchestrator and worker), realtime, the
/// test runner and the healthcheck.
pub const VALKEY_USERS: [&str; 5] = ["proxy", "control", "realtime", "runner", "health"];

#[cfg(any(test, feature = "test-support"))]
/// Creates every missing secret under `keys`; keeps the others as they are. Refuses an existing
/// empty file (naming it) and leaves it untouched.
pub fn ensure(keys: &Path) -> Result<()> {
    for (file, kind) in SECRET_FILES {
        ensure_one(&keys.join(file), *kind)?;
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
fn ensure_one(path: &Path, kind: Kind) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() > 0 => return Ok(()),
        Ok(meta) if meta.is_file() => anyhow::bail!(
            "{} is empty; a service may hold its old value, so it is not replaced: delete it \
             and run `just workbench up` again (then restart the services that read it)",
            path.display()
        ),
        Ok(_) => anyhow::bail!("{} exists and is not a file", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    }
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    let bytes = random_32();
    let contents = match kind {
        Kind::Text => {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(bytes)
                .into_bytes()
        }
        Kind::RawKey => bytes.to_vec(),
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    // create_new: a file that appears meanwhile (a concurrent `up`) is never overwritten.
    let mut file = match options.open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
    };
    use std::io::Write;
    if let Err(e) = file.write_all(&contents).and_then(|_| file.sync_all()) {
        // Never leave a half-written or empty secret behind.
        let _ = std::fs::remove_file(path);
        return Err(e).with_context(|| format!("writing {}", path.display()));
    }
    tracing::info!("wrote the service secret {}", path.display());
    Ok(())
}

/// `dir` and its parents, the new ones owner-only.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))
}

#[cfg(any(test, feature = "test-support"))]
/// 32 bytes from the OS random number generator (through the secrets key generator, so the CLI
/// needs no RNG crate of its own).
fn random_32() -> [u8; 32] {
    use base64::Engine;
    let encoded = chelabase_secrets::SecretKey::generate().to_base64();
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("SecretKey::to_base64 is standard base64")
        .try_into()
        .expect("an AES-256 key is 32 bytes")
}

#[cfg(any(test, feature = "test-support"))]
/// A text secret's value: the file without its trailing line break; empty is an error.
pub fn read_text(keys: &Path, file: &str) -> Result<String> {
    let path = keys.join(file);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let value = text.trim_end_matches(['\n', '\r']);
    anyhow::ensure!(!value.is_empty(), "{} is empty", path.display());
    Ok(value.to_string())
}
