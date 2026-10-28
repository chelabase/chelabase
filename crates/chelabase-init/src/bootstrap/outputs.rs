//! The bootstrap's output dir: the one place it writes.
//!
//! Every name is reached through an open directory handle ([`crate::fsx::Dir`]): a name that
//! is a symlink (or anything but a regular file) is refused, never followed or replaced, and
//! the dir itself must not be a symlink. A file is written to a temporary file created
//! `O_EXCL | O_NOFOLLOW` at its final mode, then renamed into place.

use anyhow::{Result, bail};
use std::path::Path;

use crate::fsx::{Dir, Kind, Perm};

/// The Zitadel org the tenant is bound to.
pub const ORG_ID: &str = "org-id";
/// The `chelabase` project.
pub const PROJECT_ID: &str = "project-id";
/// The API app's client id (a token audience).
pub const API_CLIENT_ID: &str = "api-client-id";
/// Studio's OIDC client id.
pub const STUDIO_CLIENT_ID: &str = "studio-client-id";
/// The Actions target's signing key (secret): Zitadel shows it only when the target is made.
pub const ACTIONS_SIGNING_KEY: &str = "actions-signing-key";
/// `--demo`: the demo machine user's id.
pub const DEMO_USER_ID: &str = "demo-user-id";
/// `--demo`: the demo machine user's key file (secret).
pub const DEMO_KEY: &str = "demo-sa.json";
/// A digest of the SMTP password last sent to Zitadel (secret-derived), so a changed password is
/// sent again and an unchanged one isn't.
pub const SMTP_PASSWORD_DIGEST: &str = "smtp-password.sha256";
/// Serialises runs on one output dir.
pub const LOCK: &str = ".chelabase-bootstrap.lock";

/// Every name the bootstrap reads or writes in its output dir.
pub const ALL: [&str; 8] = [
    ORG_ID,
    PROJECT_ID,
    API_CLIENT_ID,
    STUDIO_CLIENT_ID,
    ACTIONS_SIGNING_KEY,
    DEMO_USER_ID,
    DEMO_KEY,
    SMTP_PASSWORD_DIGEST,
];

/// Ids are readable by the apps; secrets by the writer only.
const PUBLIC_MODE: u32 = 0o644;
const SECRET_MODE: u32 = 0o600;

pub struct Outputs {
    dir: Dir,
}

impl Outputs {
    /// Opens (creating it 0700 when missing) the output dir and checks every name it uses:
    /// a symlink or a non-file there fails the run before anything is written.
    pub fn open(path: &Path) -> Result<Self> {
        let out = Self {
            dir: Dir::open_root(path, false)?,
        };
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
            Some(s) if s.kind == Kind::Symlink => bail!(
                "refusing {}: it is a symlink",
                self.dir.child(name).display()
            ),
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

    /// The file's text without surrounding whitespace, or `None` when it is missing or blank.
    pub fn read_value(&self, name: &str) -> Result<Option<String>> {
        Ok(self
            .read(name)?
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .filter(|v| !v.is_empty()))
    }

    /// Writes an id (`value` and a newline, 0644) unless the file already holds it: a rerun
    /// writes nothing. Returns whether it wrote.
    pub fn write_value(&self, name: &str, value: &str) -> Result<bool> {
        let contents = format!("{value}\n");
        if self.read(name)?.as_deref() == Some(contents.as_bytes()) {
            return Ok(false);
        }
        self.dir
            .replace(name, contents.as_bytes(), perm(PUBLIC_MODE))?;
        Ok(true)
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
    fn values_are_written_once_at_their_mode() {
        let t = tmp();
        let out = Outputs::open(t.path()).unwrap();
        assert!(out.write_value(ORG_ID, "o1").unwrap());
        assert!(
            !out.write_value(ORG_ID, "o1").unwrap(),
            "unchanged: no write"
        );
        assert_eq!(out.read_value(ORG_ID).unwrap().as_deref(), Some("o1"));
        let mode = |n: &str| {
            std::fs::metadata(t.path().join(n))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(ORG_ID), 0o644);
        out.write_secret(ACTIONS_SIGNING_KEY, b"k\n").unwrap();
        assert_eq!(mode(ACTIONS_SIGNING_KEY), 0o600);
    }
}
