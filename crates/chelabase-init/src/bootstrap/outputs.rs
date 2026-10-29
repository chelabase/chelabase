//! The bootstrap's output dir: the one place it writes.
//!
//! Every name is reached through an open directory handle ([`crate::fsx::Dir`]): a name that
//! is a symlink (or anything but a regular file) is refused, never followed or replaced, and
//! the dir itself must not be a symlink. A file is written to a temporary file created
//! `O_EXCL | O_NOFOLLOW` at its final mode, then renamed into place.
//!
//! The ids the apps read are in their own subdir, `ids/` (0700, made when missing, refused when
//! it is a symlink or not the writer's), so an app can mount that dir whole and read-only: a
//! rewritten id is seen at once (a single-file mount keeps the replaced file), and none of the
//! secrets here (the Actions signing key, the SMTP digest, the demo key) is in it. Their names
//! are `ids/<id>`, relative to the output dir.

use anyhow::{Result, bail};
use std::path::Path;

use crate::fsx::{Dir, Kind, Perm};

/// The subdir of the ids.
pub const IDS_DIR: &str = "ids";
/// The Zitadel org the tenant is bound to.
pub const ORG_ID: &str = "ids/org-id";
/// The `chelabase` project.
pub const PROJECT_ID: &str = "ids/project-id";
/// The API app's client id (a token audience).
pub const API_CLIENT_ID: &str = "ids/api-client-id";
/// Studio's OIDC client id.
pub const STUDIO_CLIENT_ID: &str = "ids/studio-client-id";
/// The Actions target's signing key (secret): Zitadel shows it only when the target is made.
pub const ACTIONS_SIGNING_KEY: &str = "actions-signing-key";
/// `--demo`: the demo machine user's id.
pub const DEMO_USER_ID: &str = "ids/demo-user-id";
/// `--demo`: the demo machine user's key file (secret).
pub const DEMO_KEY: &str = "demo-sa.json";
/// A digest of the SMTP password last sent to Zitadel (secret-derived), so a changed password is
/// sent again and an unchanged one isn't.
pub const SMTP_PASSWORD_DIGEST: &str = "smtp-password.sha256";
/// Serialises runs on one output dir.
pub const LOCK: &str = ".chelabase-bootstrap.lock";

/// The ids the apps read.
pub const IDS: [&str; 5] = [ORG_ID, PROJECT_ID, API_CLIENT_ID, STUDIO_CLIENT_ID, DEMO_USER_ID];

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
    ids: Dir,
}

impl Outputs {
    /// Opens (creating it 0700 when missing) the output dir and its `ids/`, and checks every
    /// name it uses: a symlink or a non-file there fails the run before anything is written.
    pub fn open(path: &Path) -> Result<Self> {
        let dir = Dir::open_root(path, false)?;
        let ids = match dir.stat(IDS_DIR)? {
            None => dir.create_dir(IDS_DIR, perm(0o700))?,
            // The output dir's owner and mode rules (and never through a symlink).
            Some(s) if s.kind == Kind::Dir => Dir::open_root(&dir.child(IDS_DIR), false)?,
            Some(s) if s.kind == Kind::Symlink => bail!(
                "refusing {}: it is a symlink",
                dir.child(IDS_DIR).display()
            ),
            Some(_) => bail!(
                "refusing {}: it is not a directory",
                dir.child(IDS_DIR).display()
            ),
        };
        let out = Self { dir, ids };
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

    /// The dir that holds `name` and the name in it (`ids/<id>` is in `ids/`).
    fn at<'a>(&self, name: &'a str) -> (&Dir, &'a str) {
        match name.strip_prefix("ids/") {
            Some(id) => (&self.ids, id),
            None => (&self.dir, name),
        }
    }

    fn check(&self, name: &str) -> Result<bool> {
        let (dir, name) = self.at(name);
        match dir.stat(name)? {
            None => Ok(false),
            Some(s) if s.kind == Kind::File => Ok(true),
            Some(s) if s.kind == Kind::Symlink => {
                bail!("refusing {}: it is a symlink", dir.child(name).display())
            }
            Some(_) => bail!(
                "refusing {}: it is not a regular file",
                dir.child(name).display()
            ),
        }
    }

    /// The file's bytes, or `None` when it doesn't exist.
    pub fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        if !self.check(name)? {
            return Ok(None);
        }
        let (dir, name) = self.at(name);
        dir.read(name).map(Some)
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
        let (dir, name) = self.at(name);
        dir.replace(name, contents.as_bytes(), perm(PUBLIC_MODE))?;
        Ok(true)
    }

    /// Writes a secret (0600), replacing what is there.
    pub fn write_secret(&self, name: &str, contents: &[u8]) -> Result<()> {
        self.check(name)?;
        let (dir, name) = self.at(name);
        dir.replace(name, contents, perm(SECRET_MODE))
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

    // The ids are in their own dir (ids/), which the apps mount whole and read-only: a
    // rewritten id (a new file renamed into place) is seen at once, and no secret is in it.
    #[test]
    fn ids_are_in_their_own_dir_without_the_secrets() {
        let t = tmp();
        let out = Outputs::open(t.path()).unwrap();
        for id in IDS {
            assert!(id.starts_with("ids/"), "{id}");
            out.write_value(id, "v1").unwrap();
        }
        out.write_secret(ACTIONS_SIGNING_KEY, b"k\n").unwrap();
        out.write_secret(SMTP_PASSWORD_DIGEST, b"d\n").unwrap();
        out.write_secret(DEMO_KEY, b"{}").unwrap();
        let ids = t.path().join("ids");
        let mut names: Vec<_> = std::fs::read_dir(&ids)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            ["api-client-id", "demo-user-id", "org-id", "project-id", "studio-client-id"]
        );
        assert_eq!(
            std::fs::metadata(&ids).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // A rewrite replaces the file: a reader of the dir sees the new value.
        out.write_value(PROJECT_ID, "v2").unwrap();
        assert_eq!(
            std::fs::read_to_string(t.path().join(PROJECT_ID)).unwrap(),
            "v2\n"
        );
    }

    // ids/ as a symlink (or not a dir) is refused before anything is written.
    #[test]
    fn a_symlinked_ids_dir_is_refused() {
        let t = tmp();
        let elsewhere = tmp();
        std::os::unix::fs::symlink(elsewhere.path(), t.path().join("ids")).unwrap();
        let err = format!("{:#}", Outputs::open(t.path()).err().unwrap());
        assert!(err.contains("symlink"), "{err}");
        let t = tmp();
        std::fs::write(t.path().join("ids"), "x").unwrap();
        assert!(Outputs::open(t.path()).is_err());
    }
}
