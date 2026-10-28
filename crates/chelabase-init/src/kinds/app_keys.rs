//! The app-user JWT key pair (`app-jwt.key` / `app-jwt.pub`) in the auth dir: `auth` signs app
//! tokens with the private key, the data API's gateway verifies with the public one.

use crate::kinds::internal_keys::write_private;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const PRIVATE_FILE: &str = "app-jwt.key";
pub const PUBLIC_FILE: &str = "app-jwt.pub";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppKeyPaths {
    pub private: PathBuf,
    pub public: PathBuf,
}

/// Create the pair in `dir` when neither file exists; reuse it when both do; never overwrite.
pub fn ensure(dir: &Path) -> Result<AppKeyPaths> {
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?;
    let paths = AppKeyPaths {
        private: dir.join(PRIVATE_FILE),
        public: dir.join(PUBLIC_FILE),
    };
    match (paths.private.is_file(), paths.public.is_file()) {
        (true, true) => tracing::info!("reusing app JWT key pair in {}", dir.display()),
        (false, false) => {
            let (private_pem, public_pem) = chelabase_auth::internal::generate_keypair_pem();
            write_private(&paths.private, &private_pem)?;
            std::fs::write(&paths.public, public_pem)
                .with_context(|| format!("write {}", paths.public.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&paths.public, std::fs::Permissions::from_mode(0o644))?;
            }
            tracing::info!("wrote app JWT key pair to {}", dir.display());
        }
        _ => anyhow::bail!(
            "only one of {PRIVATE_FILE} / {PUBLIC_FILE} exists in {}; delete both to regenerate",
            dir.display()
        ),
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_keys_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = ensure(dir.path()).unwrap();
        assert!(first.private.is_absolute());
        let private = std::fs::read(&first.private).unwrap();
        let public = std::fs::read(&first.public).unwrap();
        assert!(String::from_utf8_lossy(&private).contains("PRIVATE KEY"));
        assert!(String::from_utf8_lossy(&public).contains("PUBLIC KEY"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&first.private), 0o600);
            assert_eq!(mode(&first.public), 0o644);
        }
        let second = ensure(dir.path()).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&second.private).unwrap(), private);
        assert_eq!(std::fs::read(&second.public).unwrap(), public);
    }

    #[test]
    fn app_keys_one_missing_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ensure(dir.path()).unwrap();
        std::fs::remove_file(&paths.private).unwrap();
        let err = ensure(dir.path()).unwrap_err().to_string();
        assert!(err.contains("delete both"), "{err}");
        assert!(!paths.private.exists());
    }
}
