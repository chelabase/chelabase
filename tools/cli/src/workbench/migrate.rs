//! Main's one-time move to the workbench layout (W2 spec section 6): the state that belongs to
//! main's stack goes under `.chelabase/stacks/main/`, the host-app leftovers go. Idempotent: a
//! second run finds nothing to do. The machine keys (`.chelabase/{secrets,gateway,auth,neon}`)
//! stay where they are. Stopping the legacy dev processes is the caller's job (before this
//! removes `.chelabase/dev/`).

use anyhow::Context as _;
use std::path::{Component, Path, PathBuf};

/// One move from the old layout to main's stack dir (paths relative to `<main>/.chelabase`).
const MOVES: &[(&str, &str)] = &[
    ("neon/proxy-ca.crt", "stacks/main/neon/proxy-ca.crt"),
];

/// Host-app state that no longer exists (relative to `<main>/.chelabase`).
const RETIRED_DIRS: &[&str] = &["dev", "live"];

/// Studio's old env file in a checkout.
const STUDIO_ENV: &str = "apps/studio/.env.local";

/// Moves main's old layout into `.chelabase/stacks/main/` and removes what the workbench
/// retired; returns one line per action taken (empty when there was nothing to do). When an old
/// path and its new place both exist, nothing moves and the error names both.
pub fn migrate_main_layout(main: &Path) -> anyhow::Result<Vec<String>> {
    let base = main.join(".chelabase");
    let mut done = Vec::new();

    // Every conflict is found before anything moves.
    for (old, new) in MOVES {
        let (from, to) = (base.join(old), base.join(new));
        if exists(&from) && exists(&to) {
            anyhow::bail!(
                "both .chelabase/{old} and .chelabase/{new} exist in {}: the workbench uses the \
                 second; move or delete the first after checking it, then run up again",
                main.display()
            );
        }
    }

    for (old, new) in MOVES {
        let (from, to) = (base.join(old), base.join(new));
        if !exists(&from) {
            continue;
        }
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::rename(&from, &to)
            .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        done.push(format!("moved .chelabase/{old} to .chelabase/{new}"));
    }

    for dir in RETIRED_DIRS {
        let path = base.join(dir);
        if exists(&path) {
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing {}", path.display()))?;
            done.push(format!("removed .chelabase/{dir}/"));
        }
    }

    let studio_env = main.join(STUDIO_ENV);
    if exists(&studio_env) {
        std::fs::remove_file(&studio_env)
            .with_context(|| format!("removing {}", studio_env.display()))?;
        done.push(format!(
            "removed {STUDIO_ENV} (Studio's env is now <stack dir>/studio.env)"
        ));
    }

    // Worktrees linked their Studio env to main's; a real file there isn't ours to delete.
    let worktrees = main.join(".worktrees");
    if worktrees.is_dir() {
        let entries = std::fs::read_dir(&worktrees)
            .with_context(|| format!("reading {}", worktrees.display()))?;
        for item in entries {
            let item = item.with_context(|| format!("reading {}", worktrees.display()))?;
            let link = item.path().join(STUDIO_ENV);
            if links_to(&link, &studio_env) {
                std::fs::remove_file(&link)
                    .with_context(|| format!("removing {}", link.display()))?;
                done.push(format!("removed the link {}", link.display()));
            }
        }
    }
    Ok(done)
}

/// Whether `p` exists as anything, a dangling symlink included.
fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Whether `link` is a symlink whose target is `target` (absolute, or relative to the link's
/// directory), compared without touching the file system (the target may already be gone).
fn links_to(link: &Path, target: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(link) else {
        return false;
    };
    if !meta.file_type().is_symlink() {
        return false;
    }
    let Ok(dest) = std::fs::read_link(link) else {
        return false;
    };
    let dest = match link.parent() {
        Some(dir) if dest.is_relative() => dir.join(dest),
        _ => dest,
    };
    normalize(&dest) == normalize(target)
}

/// `p` with `.` dropped and `..` applied, lexically.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_link_to_mains_studio_env_is_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let m = tmp.path();
        std::fs::create_dir_all(m.join("apps/studio")).unwrap();
        std::fs::write(m.join(STUDIO_ENV), "x").unwrap();
        let dir = m.join(".worktrees/rel/apps/studio");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("../../../../apps/studio/.env.local", dir.join(".env.local"))
            .unwrap();
        let elsewhere = m.join(".worktrees/other/apps/studio");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink("/somewhere/else", elsewhere.join(".env.local")).unwrap();

        migrate_main_layout(m).unwrap();
        assert!(std::fs::symlink_metadata(dir.join(".env.local")).is_err());
        assert!(
            std::fs::symlink_metadata(elsewhere.join(".env.local")).is_ok(),
            "a link to another file stays"
        );
    }

    #[test]
    fn a_proxy_ca_conflict_moves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let m = tmp.path();
        let base = m.join(".chelabase");
        std::fs::create_dir_all(base.join("neon")).unwrap();
        std::fs::create_dir_all(base.join("stacks/main/neon")).unwrap();
        std::fs::write(base.join("neon/proxy-ca.crt"), "old").unwrap();
        std::fs::write(base.join("stacks/main/neon/proxy-ca.crt"), "new").unwrap();
        let err = migrate_main_layout(m).unwrap_err().to_string();
        assert!(err.contains(".chelabase/neon/proxy-ca.crt"), "{err}");
    }
}
