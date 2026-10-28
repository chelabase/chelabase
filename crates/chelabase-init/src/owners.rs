//! The ownership map: each layout entry's owner uid:gid and mode.
//!
//! The defaults are the `spec` column of [`crate::secrets::LAYOUT`]. An override file
//! (`chelabase-init secrets --owners <file>`) changes them for another runtime:
//!
//! ```json
//! {
//!   "all": {"uid": 10001, "gid": 10001},
//!   "files": {"db/metadata.password": {"gid": 70, "mode": "0640"}}
//! }
//! ```
//!
//! `all` sets the owner and group of every entry (its mode is kept); `files` then sets single
//! entries by their path in the layout. A field left out keeps its value; a path the layout
//! doesn't have is an error. Directories are not listed: each gets its mode from the files below
//! it (see [`crate::secrets`]).

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;

use crate::fsx::Perm;

/// A uid or gid: the writer's own (the process running `chelabase-init`), or a fixed number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Id {
    Writer,
    Is(u32),
}

/// One entry's owner, group and mode before the writer's ids are filled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    pub uid: Id,
    pub gid: Id,
    pub mode: u32,
}

impl Spec {
    /// Owner-only, the writer's: every secret no other uid reads.
    pub const PRIVATE: Spec = Spec {
        uid: Id::Writer,
        gid: Id::Writer,
        mode: 0o600,
    };
    /// A public key or certificate.
    pub const PUBLIC: Spec = Spec {
        uid: Id::Writer,
        gid: Id::Writer,
        mode: 0o644,
    };

    /// Readable by the group `gid` too: a reader whose uid differs from the writer's.
    pub const fn group(gid: u32) -> Spec {
        Spec {
            uid: Id::Writer,
            gid: Id::Is(gid),
            mode: 0o640,
        }
    }

    /// With the writer's ids filled in.
    pub fn resolve(&self) -> Perm {
        let (uid, gid) = crate::fsx::writer();
        let pick = |id: Id, own: u32| match id {
            Id::Writer => own,
            Id::Is(n) => n,
        };
        Perm {
            uid: pick(self.uid, uid),
            gid: pick(self.gid, gid),
            mode: self.mode,
        }
    }
}

/// The spec of every file in the layout, by its path relative to its root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnershipMap {
    files: BTreeMap<String, Spec>,
}

impl Default for OwnershipMap {
    /// The self-hosted defaults (the layout's own column).
    fn default() -> Self {
        OwnershipMap {
            files: crate::secrets::LAYOUT
                .iter()
                .map(|e| (e.path.to_string(), e.spec))
                .collect(),
        }
    }
}

impl OwnershipMap {
    /// Every entry owned by the writer, and readable by no other uid or group: the workbench,
    /// where every container runs as the stack's uid (a 0640 default becomes 0600).
    pub fn writer_only() -> Self {
        let mut map = Self::default();
        for spec in map.files.values_mut() {
            *spec = if spec.mode & 0o004 != 0 {
                Spec::PUBLIC
            } else {
                Spec::PRIVATE
            };
        }
        map
    }

    /// Every entry's owner and group set to `uid` and `gid`, modes kept.
    pub fn with_all(mut self, uid: Id, gid: Id) -> Self {
        for spec in self.files.values_mut() {
            spec.uid = uid;
            spec.gid = gid;
        }
        self
    }

    /// The spec of `path`; every layout path has one.
    pub fn spec(&self, path: &str) -> Spec {
        *self
            .files
            .get(path)
            .unwrap_or_else(|| panic!("{path} is not in the layout"))
    }

    /// The defaults with an override file's changes (see the module doc).
    pub fn from_override_json(text: &str) -> Result<Self> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Change {
            uid: Option<u32>,
            gid: Option<u32>,
            mode: Option<String>,
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            all: Option<Change>,
            #[serde(default)]
            files: BTreeMap<String, Change>,
        }
        let file: File = serde_json::from_str(text).context("the owners file is not valid")?;
        let mut map = Self::default();
        let apply = |spec: &mut Spec, c: &Change, what: &str| -> Result<()> {
            if let Some(uid) = c.uid {
                spec.uid = Id::Is(uid);
            }
            if let Some(gid) = c.gid {
                spec.gid = Id::Is(gid);
            }
            if let Some(mode) = &c.mode {
                let m = u32::from_str_radix(mode.trim_start_matches("0o"), 8)
                    .with_context(|| format!("{what}: mode {mode:?} is not octal"))?;
                // Never writable by anyone but the owner, never executable or set-id.
                if m & !0o644 != 0 || m & 0o600 != 0o600 {
                    bail!(
                        "{what}: mode {mode} must be read-write for the owner and at most readable by others"
                    );
                }
                spec.mode = m;
            }
            Ok(())
        };
        if let Some(all) = &file.all {
            anyhow::ensure!(all.mode.is_none(), "`all` sets uid and gid only, not mode");
            for spec in map.files.values_mut() {
                apply(spec, all, "all")?;
            }
        }
        for (path, change) in &file.files {
            let Some(spec) = map.files.get_mut(path) else {
                bail!("the owners file names {path}, which is not in the layout");
            };
            apply(spec, change, path)?;
        }
        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_changes_only_what_it_names() {
        let map = OwnershipMap::from_override_json(
            r#"{"all": {"uid": 5}, "files": {"db/metadata.password": {"gid": 7, "mode": "0600"}}}"#,
        )
        .unwrap();
        assert_eq!(
            map.spec("db/metadata.password"),
            Spec {
                uid: Id::Is(5),
                gid: Id::Is(7),
                mode: 0o600
            }
        );
        assert_eq!(map.spec("secrets/secrets.key").uid, Id::Is(5));
        assert_eq!(map.spec("secrets/secrets.key").gid, Id::Writer);
    }

    #[test]
    fn an_override_refuses_unknown_paths_and_loose_modes() {
        for bad in [
            r#"{"files": {"nope": {"uid": 1}}}"#,
            r#"{"files": {"db/metadata.password": {"mode": "0666"}}}"#,
            r#"{"files": {"db/metadata.password": {"mode": "0400"}}}"#,
            r#"{"all": {"mode": "0600"}}"#,
            r#"{"other": 1}"#,
        ] {
            assert!(OwnershipMap::from_override_json(bad).is_err(), "{bad}");
        }
    }
}
