//! Control-plane secrets key (encrypts database passwords at rest).

use anyhow::Result;
use std::path::{Path, PathBuf};

pub const SECRETS_KEY_FILE: &str = "secrets.key";

/// Create `dir/secrets.key` once (0600) and return its absolute path. Never
/// overwrites: existing encrypted passwords depend on this key.
pub fn ensure(dir: &Path) -> Result<PathBuf> {
    use anyhow::Context;
    use std::io::Write;

    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let path = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?
        .join(SECRETS_KEY_FILE);
    if path.is_file() {
        tracing::info!("reusing secrets key {}", path.display());
        return Ok(path);
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(
        cheladb_secrets::SecretKey::generate()
            .to_base64()
            .as_bytes(),
    )
    .with_context(|| format!("write {}", path.display()))?;
    tracing::info!("wrote secrets key {}", path.display());
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cheladb_secrets::{open, seal, SecretKey};

    #[test]
    fn creates_a_loadable_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = ensure(dir.path()).unwrap();
        assert!(path.is_absolute());
        let key = SecretKey::from_file(&path).unwrap();
        assert_eq!(open(&key, "db", &seal(&key, "db", "pw")).unwrap(), "pw");
    }

    #[test]
    fn second_call_keeps_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let first = std::fs::read(ensure(dir.path()).unwrap()).unwrap();
        let second = std::fs::read(ensure(dir.path()).unwrap()).unwrap();
        assert_eq!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = ensure(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
