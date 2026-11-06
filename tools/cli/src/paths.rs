use std::env;
use std::path::PathBuf;

/// Repo root: `CHELABASE_ROOT`, else walk up from cwd for `justfile`.
pub fn repo_root() -> PathBuf {
    if let Ok(root) = env::var("CHELABASE_ROOT") {
        return PathBuf::from(root);
    }
    let Ok(mut dir) = env::current_dir() else {
        return PathBuf::from(".");
    };
    loop {
        if dir.join("justfile").is_file() {
            return dir;
        }
        if !dir.pop() {
            break;
        }
    }
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// The stack's bootstrap dir (`CHELABASE_BOOTSTRAP_DIR`, which the stack env sets, else
/// `$CHELABASE_STACK_DIR/bootstrap`, else the main stack's): the dev member's password, for
/// `chelabase dev token`.
pub fn bootstrap_dir() -> PathBuf {
    if let Some(dir) = env::var_os("CHELABASE_BOOTSTRAP_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(stack) = env::var_os("CHELABASE_STACK_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(stack).join("bootstrap");
    }
    repo_root().join(".chelabase/stacks/main/bootstrap")
}

/// Where auth's cookie key lives. Before the rename it was `.chelabase/ui-auth`; that folder is
/// moved once, so the existing key survives and signed-in users stay signed in. When both
/// exist, `auth/` wins and the old one is left alone.
pub fn migrate_auth_dir(root: &std::path::Path) -> std::io::Result<PathBuf> {
    let old = root.join(".chelabase/ui-auth");
    let new = root.join(".chelabase/auth");
    match (old.exists(), new.exists()) {
        (true, false) => {
            std::fs::rename(&old, &new)?;
            tracing::info!("moved {} to {}", old.display(), new.display());
        }
        (true, true) => tracing::warn!(
            "both {} and {} exist; using the second. Delete the first once you've checked it.",
            old.display(),
            new.display()
        ),
        _ => {}
    }
    Ok(new)
}

/// Where `chelabase dev bootstrap` keeps the compute-JWT key pair and the step 3 demo files.
pub fn neon_dir() -> PathBuf {
    if let Ok(dir) = env::var("CHELABASE_NEON_DIR") {
        return PathBuf::from(dir);
    }
    repo_root().join(".chelabase/neon")
}

/// The public entry (hosts, scheme) from the `CHELABASE_*_HOST` variables.
pub fn entry() -> anyhow::Result<chelabase_types::entry::Entry> {
    Ok(chelabase_types::entry::Entry::from_env()?)
}


/// Parse KEY=VALUE lines from a dotenv-style file.
pub fn load_dotenv_file(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    // Review Focus 1.
    #[test]
    fn the_old_folder_is_moved_with_its_key() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(".chelabase/ui-auth");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("cookie.key"), "the-key").unwrap();

        let dir = migrate_auth_dir(root.path()).unwrap();
        assert_eq!(dir, root.path().join(".chelabase/auth"));
        assert_eq!(std::fs::read_to_string(dir.join("cookie.key")).unwrap(), "the-key");
        assert!(!old.exists());
    }

    // Review Focus 2.
    #[test]
    fn when_both_exist_the_new_one_wins_and_nothing_is_overwritten() {
        let root = tempfile::tempdir().unwrap();
        for (name, key) in [("ui-auth", "old-key"), ("auth", "new-key")] {
            let d = root.path().join(".chelabase").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("cookie.key"), key).unwrap();
        }
        let dir = migrate_auth_dir(root.path()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("cookie.key")).unwrap(), "new-key");
        assert_eq!(
            std::fs::read_to_string(root.path().join(".chelabase/ui-auth/cookie.key")).unwrap(),
            "old-key"
        );
    }

    #[test]
    fn a_fresh_checkout_just_gets_the_new_path() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(migrate_auth_dir(root.path()).unwrap(), root.path().join(".chelabase/auth"));
    }
}
