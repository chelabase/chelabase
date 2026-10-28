//! The key pair the control plane signs compute_ctl tokens with (`ComputeSigner`).

use anyhow::{Context, Result};
use chelabase_neon::jwt::{PRIVATE_KEY_FILE, PUBLIC_KEY_FILE, generate_keypair};
use std::path::{Path, PathBuf};

use crate::kinds::internal_keys::write_private;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeJwtPaths {
    pub dir: PathBuf,
    pub private: PathBuf,
    pub public: PathBuf,
}

/// Create the key pair in `dir` if neither file exists. Never overwrites: running computes
/// trust the public key they were started with.
pub fn ensure(dir: &Path) -> Result<ComputeJwtPaths> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?;
    let paths = ComputeJwtPaths {
        private: dir.join(PRIVATE_KEY_FILE),
        public: dir.join(PUBLIC_KEY_FILE),
        dir: dir.clone(),
    };
    match (paths.private.is_file(), paths.public.is_file()) {
        (true, true) => tracing::info!("reusing the compute-JWT key pair in {}", dir.display()),
        (false, false) => {
            let (pem, x) = generate_keypair();
            write_pair(&paths, &pem, &format!("{x}\n"))?;
            tracing::info!("wrote the compute-JWT key pair to {}", dir.display());
        }
        _ => anyhow::bail!(
            "only one of {PRIVATE_KEY_FILE} / {PUBLIC_KEY_FILE} exists in {}; delete both to regenerate",
            dir.display()
        ),
    }
    Ok(paths)
}

/// A temporary name next to `path`, unique per run so concurrent runs never share one.
pub fn temp_name(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let mut name = path.as_os_str().to_owned();
    name.push(format!(
        ".{}-{nanos:x}-{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    PathBuf::from(name)
}

/// Writes both files under temporary names, then publishes them without ever replacing an
/// existing file, so a failure leaves no half pair behind and a key is never overwritten.
fn write_pair(paths: &ComputeJwtPaths, pem: &str, public: &str) -> Result<()> {
    let (private_tmp, public_tmp) = (temp_name(&paths.private), temp_name(&paths.public));
    let result = (|| -> Result<()> {
        write_private(&private_tmp, pem)?;
        std::fs::write(&public_tmp, public)
            .with_context(|| format!("write {}", public_tmp.display()))?;
        publish(paths, &private_tmp, &public_tmp)
    })();
    let _ = std::fs::remove_file(&private_tmp);
    let _ = std::fs::remove_file(&public_tmp);
    result
}

/// Hard-links each temporary to its final name: that fails with AlreadyExists instead of
/// replacing a file, a dangling symlink included. If the public key can't be published, the
/// private key this run just published is removed again.
fn publish(paths: &ComputeJwtPaths, private_tmp: &Path, public_tmp: &Path) -> Result<()> {
    std::fs::hard_link(private_tmp, &paths.private)
        .with_context(|| format!("create {}", paths.private.display()))?;
    if let Err(e) = std::fs::hard_link(public_tmp, &paths.public)
        .with_context(|| format!("create {}", paths.public.display()))
    {
        let _ = std::fs::remove_file(&paths.private);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_neon::ComputeSigner;

    fn signer(paths: &ComputeJwtPaths) -> anyhow::Result<ComputeSigner> {
        ComputeSigner::new(
            &std::fs::read_to_string(&paths.private)?,
            &std::fs::read_to_string(&paths.public)?,
        )
    }

    #[test]
    fn creates_a_key_pair_the_signer_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        assert!(paths.dir.is_absolute());
        assert!(signer(&paths).is_ok());
    }

    #[test]
    fn a_second_run_keeps_the_keys() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure(dir.path()).unwrap();
        let before = std::fs::read(&first.private).unwrap();
        let second = ensure(dir.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second.private).unwrap(), before);
    }

    #[test]
    fn half_a_key_pair_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        std::fs::remove_file(&paths.public).unwrap();
        let err = ensure(dir.path()).unwrap_err().to_string();
        assert!(err.contains("delete both"), "{err}");
        assert!(
            !paths.public.exists(),
            "must not regenerate half a key pair"
        );
    }

    #[test]
    fn a_failed_write_leaves_no_half_pair_or_temporaries() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where the public key belongs makes the last rename fail.
        std::fs::create_dir(dir.path().join(PUBLIC_KEY_FILE)).unwrap();
        assert!(ensure(dir.path()).is_err());
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, [std::ffi::OsString::from(PUBLIC_KEY_FILE)]);
    }

    #[test]
    fn a_key_that_appears_before_publishing_is_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ComputeJwtPaths {
            dir: dir.path().to_path_buf(),
            private: dir.path().join(PRIVATE_KEY_FILE),
            public: dir.path().join(PUBLIC_KEY_FILE),
        };
        let (private_tmp, public_tmp) = (temp_name(&paths.private), temp_name(&paths.public));
        assert_ne!(private_tmp, temp_name(&paths.private));
        write_private(&private_tmp, "new").unwrap();
        std::fs::write(&public_tmp, "new").unwrap();
        // Someone else creates the private key after the temporaries were written.
        std::fs::write(&paths.private, "theirs").unwrap();

        assert!(publish(&paths, &private_tmp, &public_tmp).is_err());
        assert_eq!(std::fs::read_to_string(&paths.private).unwrap(), "theirs");
        assert!(!paths.public.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        let mode = std::fs::metadata(&paths.private)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
