//! The bootstrap's output dir: the one place it writes.
//!
//! Every name is reached through an open directory handle ([`crate::fsx::Dir`]): a name that
//! is a symlink (or anything but a regular file) is refused, never followed or replaced, and
//! the dir itself must not be a symlink. A file is written to a temporary file created
//! `O_EXCL | O_NOFOLLOW` at its final mode, then renamed into place.

use anyhow::{Result, bail};
use std::path::Path;

use crate::fsx::{Dir, Kind, Perm};

/// `--demo`: the dev member's email and password (secret), for `chelabase dev token`.
pub const DEV_MEMBER: &str = "dev-member.json";
/// Serialises runs on one output dir.
pub const LOCK: &str = ".chelabase-bootstrap.lock";

/// Every name the bootstrap reads or writes in its output dir.
pub const ALL: [&str; 1] = [DEV_MEMBER];

/// Secrets are readable by the writer only.
const SECRET_MODE: u32 = 0o600;

pub struct Outputs {
    dir: Dir,
}

impl Outputs {
    /// Opens (creating it 0700 when missing) the output dir and checks every name it uses: a
    /// symlink or a non-file there fails the run before anything is written.
    pub fn open(path: &Path) -> Result<Self> {
        let out = Self { dir: Dir::open_root(path, false)? };
        for name in [LOCK].iter().chain(ALL.iter()) {
            out.check(name)?;
        }
        Ok(out)
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The dir's lock if no other run holds it (`None`), held until the file is dropped.
    pub fn try_lock(&self) -> Result<Option<std::fs::File>> {
        self.dir.try_lock(LOCK)
    }

    fn check(&self, name: &str) -> Result<bool> {
        match self.dir.stat(name)? {
            None => Ok(false),
            Some(s) if s.kind == Kind::File => Ok(true),
            Some(s) if s.kind == Kind::Symlink => {
                bail!("refusing {}: it is a symlink", self.dir.child(name).display())
            }
            Some(_) => bail!(
                "refusing {}: it is not a regular file",
                self.dir.child(name).display()
            ),
        }
    }

    /// The file's bytes, or `None` when it doesn't exist.
    pub fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        if !self.check(name)? {
            return Ok(None);
        }
        self.dir.read(name).map(Some)
    }

    /// Writes a secret (0600), replacing what is there.
    pub fn write_secret(&self, name: &str, contents: &[u8]) -> Result<()> {
        self.check(name)?;
        self.dir.replace(name, contents, perm(SECRET_MODE))
    }
}

fn perm(mode: u32) -> Perm {
    let (uid, gid) = crate::fsx::writer();
    Perm { uid, gid, mode }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp() -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        t
    }

    #[test]
    fn secrets_are_written_owner_only() {
        let t = tmp();
        let out = Outputs::open(t.path()).unwrap();
        assert_eq!(out.read(DEV_MEMBER).unwrap(), None);
        out.write_secret(DEV_MEMBER, b"{}").unwrap();
        assert_eq!(out.read(DEV_MEMBER).unwrap().as_deref(), Some(&b"{}"[..]));
        let mode = std::fs::metadata(t.path().join(DEV_MEMBER)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // A symlink at a name is refused before anything is written.
    #[test]
    fn a_symlinked_name_is_refused() {
        let t = tmp();
        let elsewhere = tmp();
        std::os::unix::fs::symlink(elsewhere.path().join("x"), t.path().join(DEV_MEMBER)).unwrap();
        let err = format!("{:#}", Outputs::open(t.path()).err().unwrap());
        assert!(err.contains("symlink"), "{err}");
    }
}
