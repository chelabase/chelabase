//! Gateway internal-identity signing keypair for local dev.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const SIGNING_KEY_FILE: &str = "internal-signing.pem";
pub const VERIFY_KEY_FILE: &str = "internal-verify.pem";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalKeyPaths {
    pub signing: PathBuf,
    pub verify: PathBuf,
}

/// Create the keypair in `dir` if neither file exists. Never overwrites: a
/// running gateway and orchestrator must keep agreeing on the key.
pub fn ensure(dir: &Path) -> Result<InternalKeyPaths> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?;
    let paths = InternalKeyPaths {
        signing: dir.join(SIGNING_KEY_FILE),
        verify: dir.join(VERIFY_KEY_FILE),
    };

    match (paths.signing.is_file(), paths.verify.is_file()) {
        (true, true) => {
            tracing::info!("reusing internal identity keypair in {}", dir.display());
        }
        (false, false) => {
            let (private_pem, public_pem) = cheladb_auth::internal::generate_keypair_pem();
            write_private(&paths.signing, &private_pem)?;
            std::fs::write(&paths.verify, public_pem)
                .with_context(|| format!("write {}", paths.verify.display()))?;
            tracing::info!("wrote internal identity keypair to {}", dir.display());
        }
        _ => anyhow::bail!(
            "only one of {SIGNING_KEY_FILE} / {VERIFY_KEY_FILE} exists in {}; delete both to regenerate",
            dir.display()
        ),
    }
    Ok(paths)
}

/// Create `path` owner-only from the start (never briefly umask-readable) and
/// refuse to replace an existing file.
pub(crate) fn write_private(path: &Path, pem: &str) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(pem.as_bytes())
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cheladb_auth::internal::{AuthMethod, Identity, InternalSigner, InternalVerifier};

    #[test]
    fn creates_a_keypair_that_signs_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        assert!(paths.signing.is_absolute());

        let signer = InternalSigner::from_pem_file(&paths.signing).unwrap();
        let verifier = InternalVerifier::from_pem_file(&paths.verify).unwrap();
        let identity = Identity {
            sub: "u".into(),
            org_id: "o".into(),
            auth: AuthMethod::PlatformJwt,
            request_id: "r".into(),
            data: None,
        };
        let token = signer.sign("orchestrator", &identity).unwrap();
        assert_eq!(verifier.verify(&token, "orchestrator").unwrap().org_id, "o");
    }

    #[cfg(unix)]
    #[test]
    fn private_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        let mode = std::fs::metadata(&paths.signing).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn second_call_keeps_existing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure(dir.path()).unwrap();
        let signing_before = std::fs::read(&first.signing).unwrap();
        let verify_before = std::fs::read(&first.verify).unwrap();

        let second = ensure(dir.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second.signing).unwrap(), signing_before);
        assert_eq!(std::fs::read(&second.verify).unwrap(), verify_before);
    }

    #[test]
    fn refuses_when_only_one_key_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        std::fs::remove_file(&paths.signing).unwrap();

        let err = ensure(dir.path()).unwrap_err().to_string();
        assert!(err.contains("delete both"), "{err}");
        assert!(!paths.signing.exists(), "must not regenerate half a keypair");
    }

    // Created with create_new: a file appearing between the existence check
    // and the write (e.g. a concurrent bootstrap) is never clobbered.
    #[test]
    fn write_private_never_overwrites_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SIGNING_KEY_FILE);
        std::fs::write(&path, "original").unwrap();

        assert!(write_private(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
    }

    #[test]
    fn creates_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b");
        ensure(&nested).unwrap();
        assert!(nested.join(SIGNING_KEY_FILE).is_file());
    }
}
