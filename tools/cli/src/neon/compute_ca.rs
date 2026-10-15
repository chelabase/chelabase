//! The platform CA for compute TLS and the orchestrator's client pair (fork round 3).

use anyhow::{Context, Result};
use chelabase_neon::compute_tls::{
    CLIENT_RENEW_BEFORE, ComputeCa, ORCHESTRATOR_CN, needs_renewal, not_after, key_matches_cert, signed_by, write_client_pair,
};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::compute_jwt::temp_name;
use crate::dev::internal_keys::write_private;

const CA_CERT_FILE: &str = "ca.crt";
const CA_KEY_FILE: &str = "ca.key";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeCaPaths {
    pub dir: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// How long `ensure` waits for a racing `ensure` to finish the second file of a new pair.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Create the CA in `dir` if neither file exists. Never overwrites: every compute trusts the
/// certificate it was started with. Half a pair is refused (after waiting [`SETTLE`] for a
/// concurrent `ensure` to finish writing it). `dir` is 0700: an existing looser one is tightened.
/// Two `ensure` calls racing on a fresh directory both succeed and agree on one CA.
pub fn ensure(dir: &Path) -> Result<ComputeCaPaths> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("mkdir {}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", dir.display()))?;
    let paths = ComputeCaPaths {
        cert: dir.join(CA_CERT_FILE),
        key: dir.join(CA_KEY_FILE),
        dir: dir.clone(),
    };
    let present = || (paths.cert.is_file(), paths.key.is_file());
    if present() == (false, false) {
        let ca = ComputeCa::generate(SystemTime::now())?;
        match write_pair(&paths, &ca) {
            Ok(()) => {
                tracing::info!("wrote the compute CA to {}", dir.display());
                return Ok(paths);
            }
            // Another `ensure` won the race: use its pair.
            Err(e) if already_exists(&e) => {}
            Err(e) => return Err(e),
        }
    }
    let deadline = std::time::Instant::now() + SETTLE;
    loop {
        match present() {
            (true, true) => {
                tracing::info!("reusing the compute CA in {}", dir.display());
                return Ok(paths);
            }
            // The other `ensure` has linked one file and is about to link the other.
            _ if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => anyhow::bail!(
                "only one of {CA_CERT_FILE} / {CA_KEY_FILE} exists in {}; delete both to regenerate",
                dir.display()
            ),
        }
    }
}

fn already_exists(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

/// Writes both files under temporary names, then hard-links each to its final name (which
/// fails instead of replacing a file), so a failure leaves no half pair behind.
fn write_pair(paths: &ComputeCaPaths, ca: &ComputeCa) -> Result<()> {
    let (key_tmp, cert_tmp) = (temp_name(&paths.key), temp_name(&paths.cert));
    let result = (|| -> Result<()> {
        write_private(&key_tmp, ca.key_pem())?;
        std::fs::write(&cert_tmp, ca.cert_pem())
            .with_context(|| format!("write {}", cert_tmp.display()))?;
        std::fs::hard_link(&key_tmp, &paths.key)
            .with_context(|| format!("create {}", paths.key.display()))?;
        // The key link above succeeded, so this call owns the pair (a racing call fails there).
        if let Err(e) = std::fs::hard_link(&cert_tmp, &paths.cert)
            .with_context(|| format!("create {}", paths.cert.display()))
        {
            let _ = std::fs::remove_file(&paths.key);
            return Err(e);
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&key_tmp);
    let _ = std::fs::remove_file(&cert_tmp);
    result
}

/// Issues the orchestrator's client pair into `tls_dir` (`current` -> `v<secs>/`) when there is
/// none, when the current one has under [`CLIENT_RENEW_BEFORE`] left at `now`, or when it does
/// not follow the CA in `ca_dir`: a file missing, `ca.crt` not the CA's certificate (a
/// regenerated CA), or `client.crt` not signed by it. Returns whether a pair was written.
pub fn ensure_client_pair(ca_dir: &Path, tls_dir: &Path, now: SystemTime) -> Result<bool> {
    let ca = ComputeCa::load(ca_dir)?;
    let current = tls_dir.join("current");
    if pair_is_current(&ca, &current, now)? {
        return Ok(false);
    }
    let client = ca.issue_client(ORCHESTRATOR_CN, now)?;
    write_client_pair(tls_dir, &client, ca.cert_pem(), now)?;
    Ok(true)
}

/// Whether `current/` holds a complete pair of `ca` that isn't due for renewal. An unreadable
/// (missing) file or an unparsable certificate means "not current"; other I/O errors fail.
fn pair_is_current(ca: &ComputeCa, current: &Path, now: SystemTime) -> Result<bool> {
    let read = |name: &str| -> Result<Option<String>> {
        let path = current.join(name);
        match std::fs::read_to_string(&path) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    };
    let (Some(crt), Some(ca_crt)) = (read("client.crt")?, read("ca.crt")?) else {
        return Ok(false);
    };
    let Some(key) = read("client.key")? else {
        return Ok(false);
    };
    if ca_crt.trim() != ca.cert_pem().trim() || !key_matches_cert(&crt, &key) {
        return Ok(false);
    }
    if !signed_by(&crt, ca.cert_pem()) {
        return Ok(false);
    }
    Ok(not_after(&crt).is_ok_and(|expires| !needs_renewal(expires, now, CLIENT_RENEW_BEFORE)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, SystemTime};

    #[test]
    fn the_ca_is_made_once_and_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        let (crt, key) = (std::fs::read(&paths.cert).unwrap(), std::fs::read(&paths.key).unwrap());
        assert_eq!(std::fs::metadata(&paths.key).unwrap().permissions().mode() & 0o777, 0o600);
        let again = ensure(dir.path()).unwrap();
        assert_eq!((std::fs::read(&again.cert).unwrap(), std::fs::read(&again.key).unwrap()), (crt, key));
        std::fs::remove_file(&paths.key).unwrap();
        assert!(ensure(dir.path()).is_err(), "a half pair is refused");
    }

    #[test]
    fn the_client_pair_is_issued_then_renewed_only_when_due() {
        let ca = tempfile::tempdir().unwrap();
        let tls = tempfile::tempdir().unwrap();
        ensure(ca.path()).unwrap();
        let now = SystemTime::now();
        assert!(ensure_client_pair(ca.path(), tls.path(), now).unwrap(), "first issue");
        let current = tls.path().join("current");
        let first = std::fs::read_link(&current).unwrap();
        assert!(!ensure_client_pair(ca.path(), tls.path(), now + Duration::from_secs(86_400)).unwrap());
        assert_eq!(std::fs::read_link(&current).unwrap(), first);
        assert!(ensure_client_pair(ca.path(), tls.path(), now + Duration::from_secs(310 * 86_400)).unwrap(), "under 60 days left");
        assert_ne!(std::fs::read_link(&current).unwrap(), first);
        for f in ["client.crt", "client.key", "ca.crt"] {
            assert!(current.join(f).is_file(), "{f}");
        }
    }

    #[test]
    fn a_garbled_client_cert_is_reissued_not_an_error() {
        let ca = tempfile::tempdir().unwrap();
        let tls = tempfile::tempdir().unwrap();
        ensure(ca.path()).unwrap();
        let now = SystemTime::now();
        ensure_client_pair(ca.path(), tls.path(), now).unwrap();
        std::fs::write(tls.path().join("current/client.crt"), "garbage").unwrap();
        assert!(ensure_client_pair(ca.path(), tls.path(), now).unwrap());
    }

    #[test]
    fn a_half_ca_is_still_refused_after_the_settle_wait() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        std::fs::remove_file(&paths.cert).unwrap();
        let err = ensure(dir.path()).unwrap_err().to_string();
        assert!(err.contains("only one of"), "{err}");
    }
}
