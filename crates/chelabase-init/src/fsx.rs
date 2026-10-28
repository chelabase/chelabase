//! File operations that never follow a symlink and never overwrite a file by accident.
//!
//! Everything goes through an open directory handle ([`Dir`]): a child is reached with
//! `openat(O_NOFOLLOW)`, so a symlink anywhere below the root is refused (`ELOOP` or `ENOTDIR`)
//! instead of followed, even if it appears after a check. A file is written to a temporary file
//! in the same directory, created `O_EXCL | O_NOFOLLOW` at 0600 and owned by the writer, then
//! given its final owner and mode with `fchown`/`fchmod` (whatever the umask) and published:
//! with `linkat` when it must not exist yet (that fails instead of replacing anything) or with
//! `renameat` for a derived file that is re-rendered.

use anyhow::{Context, Result, bail};
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A resolved owner and mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Perm {
    pub uid: u32,
    pub gid: u32,
    /// The permission bits (`0o7777` at most).
    pub mode: u32,
}

impl std::fmt::Display for Perm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "owner {}:{} mode {:04o}", self.uid, self.gid, self.mode)
    }
}

/// What a name in a directory is, without following it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)` of one name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub perm: Perm,
    pub size: u64,
}

/// An open directory and the path it was opened at (for messages only).
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
    path: PathBuf,
}

fn cname(name: &str) -> Result<CString> {
    anyhow::ensure!(
        !name.is_empty() && !name.contains('/') && name != "." && name != "..",
        "{name:?} is not a single file name"
    );
    CString::new(name).context("a file name with a NUL byte")
}

fn last_os_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn is_symlink_refusal(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR))
}

/// The process's effective uid and gid: the writer's owner.
pub fn writer() -> (u32, u32) {
    // SAFETY: geteuid and getegid can't fail and touch no memory.
    unsafe { (libc::geteuid(), libc::getegid()) }
}

/// Whether this process can give a file `uid` and `gid` (root can give any; anyone else only
/// their own uid and a group they are in).
pub fn can_own(uid: u32, gid: u32) -> bool {
    let (euid, egid) = writer();
    if euid == 0 {
        return true;
    }
    if uid != euid {
        return false;
    }
    if gid == egid {
        return true;
    }
    // SAFETY: a zero-length query returns the count; the buffer then has room for it.
    let n = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if n <= 0 {
        return false;
    }
    let mut groups = vec![0 as libc::gid_t; n as usize];
    let n = unsafe { libc::getgroups(n, groups.as_mut_ptr()) };
    n > 0 && groups[..n as usize].contains(&gid)
}

impl Dir {
    /// Opens `path` as the root of a layout, creating it (and its missing parents) 0700 when it
    /// doesn't exist. `path` itself must not be a symlink; its parents are taken as they are.
    pub fn open_root(path: &Path) -> Result<Dir> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                bail!("refusing {}: it is a symlink", path.display())
            }
            Ok(meta) if !meta.is_dir() => bail!("{} is not a directory", path.display()),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(path)
                    .with_context(|| format!("creating {}", path.display()))?;
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
        let c = CString::new(path.as_os_str().as_bytes()).context("a path with a NUL byte")?;
        // SAFETY: `c` is a valid NUL-terminated string; the returned fd is checked.
        let fd = unsafe {
            libc::open(
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = last_os_error();
            if is_symlink_refusal(&e) {
                bail!("refusing {}: it is a symlink", path.display());
            }
            return Err(e).with_context(|| format!("opening {}", path.display()));
        }
        Ok(Dir {
            // SAFETY: `fd` is a fresh, owned descriptor.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path of `name` in this directory (for messages).
    pub fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// `name`'s type, owner, mode and size, or `None` when it doesn't exist. A symlink is
    /// reported as one, never followed.
    pub fn stat(&self, name: &str) -> Result<Option<Stat>> {
        let c = cname(name)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is NUL-terminated and `st` is a valid out-pointer.
        let rc = unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                c.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if rc != 0 {
            let e = last_os_error();
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(e).with_context(|| format!("reading {}", self.child(name).display()));
        }
        let kind = match st.st_mode & libc::S_IFMT {
            libc::S_IFREG => Kind::File,
            libc::S_IFDIR => Kind::Dir,
            libc::S_IFLNK => Kind::Symlink,
            _ => Kind::Other,
        };
        Ok(Some(Stat {
            kind,
            perm: Perm {
                uid: st.st_uid,
                gid: st.st_gid,
                mode: st.st_mode & 0o7777,
            },
            size: st.st_size as u64,
        }))
    }

    /// This directory again, as its own handle.
    pub fn open_dir_self(&self) -> Result<Dir> {
        // SAFETY: "." is NUL-terminated; the returned fd is checked.
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c".".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(last_os_error())
                .with_context(|| format!("opening {}", self.path.display()));
        }
        Ok(Dir {
            // SAFETY: `fd` is a fresh, owned descriptor.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            path: self.path.clone(),
        })
    }

    /// Opens the existing subdirectory `name`; a symlink there is refused.
    pub fn open_dir(&self, name: &str) -> Result<Dir> {
        let c = cname(name)?;
        let path = self.child(name);
        // SAFETY: `c` is NUL-terminated; the returned fd is checked.
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = last_os_error();
            if is_symlink_refusal(&e) {
                bail!(
                    "refusing {}: it is a symlink or not a directory",
                    path.display()
                );
            }
            return Err(e).with_context(|| format!("opening {}", path.display()));
        }
        Ok(Dir {
            // SAFETY: `fd` is a fresh, owned descriptor.
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
            path,
        })
    }

    /// Creates the subdirectory `name` with `perm` (an existing one is an error), then opens it.
    pub fn create_dir(&self, name: &str, perm: Perm) -> Result<Dir> {
        let c = cname(name)?;
        // SAFETY: `c` is NUL-terminated.
        if unsafe { libc::mkdirat(self.fd.as_raw_fd(), c.as_ptr(), 0o700) } != 0 {
            return Err(last_os_error())
                .with_context(|| format!("creating {}", self.child(name).display()));
        }
        let dir = self.open_dir(name)?;
        set_perm(&dir.fd, perm)
            .with_context(|| format!("setting {perm} on {}", dir.path.display()))?;
        Ok(dir)
    }

    /// The whole of the regular file `name`; a symlink is refused.
    pub fn read(&self, name: &str) -> Result<Vec<u8>> {
        let c = cname(name)?;
        let path = self.child(name);
        // SAFETY: `c` is NUL-terminated; the returned fd is checked.
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            let e = last_os_error();
            if e.raw_os_error() == Some(libc::ELOOP) {
                bail!("refusing {}: it is a symlink", path.display());
            }
            return Err(e).with_context(|| format!("reading {}", path.display()));
        }
        // SAFETY: `fd` is a fresh, owned descriptor.
        let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "{} is not a regular file",
            path.display()
        );
        let mut out = Vec::new();
        file.read_to_end(&mut out)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(out)
    }

    /// Writes `contents` as the new file `name` with `perm`. Returns false, leaving the
    /// existing entry untouched, when `name` already exists (a file, a symlink or anything else).
    pub fn create_new(&self, name: &str, contents: &[u8], perm: Perm) -> Result<bool> {
        let tmp = self.write_temp(name, contents, perm)?;
        let (ctmp, cdst) = (cname(&tmp)?, cname(name)?);
        // SAFETY: both names are NUL-terminated; flags 0 never follows a symlink at `tmp`.
        let rc = unsafe {
            libc::linkat(
                self.fd.as_raw_fd(),
                ctmp.as_ptr(),
                self.fd.as_raw_fd(),
                cdst.as_ptr(),
                0,
            )
        };
        let err = (rc != 0).then(last_os_error);
        self.remove(&tmp);
        match err {
            None => {
                self.sync();
                Ok(true)
            }
            Some(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Some(e) => Err(e).with_context(|| format!("creating {}", self.child(name).display())),
        }
    }

    /// Writes `contents` as `name` with `perm`, replacing what is there (a derived file).
    pub fn replace(&self, name: &str, contents: &[u8], perm: Perm) -> Result<()> {
        let tmp = self.write_temp(name, contents, perm)?;
        let (ctmp, cdst) = (cname(&tmp)?, cname(name)?);
        // SAFETY: both names are NUL-terminated; rename replaces the entry, never follows it.
        let rc = unsafe {
            libc::renameat(
                self.fd.as_raw_fd(),
                ctmp.as_ptr(),
                self.fd.as_raw_fd(),
                cdst.as_ptr(),
            )
        };
        if rc != 0 {
            let e = last_os_error();
            self.remove(&tmp);
            return Err(e).with_context(|| format!("replacing {}", self.child(name).display()));
        }
        self.sync();
        Ok(())
    }

    /// Removes the file `name` (best effort: a cleanup).
    pub fn remove(&self, name: &str) {
        if let Ok(c) = cname(name) {
            // SAFETY: `c` is NUL-terminated.
            unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) };
        }
    }

    /// Opens (creating it 0600 if needed) the lock file `name` and takes an exclusive `flock`
    /// on it, held until the returned file is dropped.
    pub fn lock(&self, name: &str) -> Result<std::fs::File> {
        let c = cname(name)?;
        let path = self.child(name);
        // SAFETY: `c` is NUL-terminated; the returned fd is checked.
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            let e = last_os_error();
            if e.raw_os_error() == Some(libc::ELOOP) {
                bail!("refusing {}: it is a symlink", path.display());
            }
            return Err(e).with_context(|| format!("opening {}", path.display()));
        }
        // SAFETY: `fd` is a fresh, owned descriptor.
        let file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    fn write_temp(&self, name: &str, contents: &[u8], perm: Perm) -> Result<String> {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let tmp = format!(
            ".{name}.{}-{}.tmp",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let c = cname(&tmp)?;
        // SAFETY: `c` is NUL-terminated; the returned fd is checked.
        let fd = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(last_os_error())
                .with_context(|| format!("creating {}", self.child(&tmp).display()));
        }
        // SAFETY: `fd` is a fresh, owned descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let result = (|| -> Result<()> {
            set_perm(&fd, perm)
                .with_context(|| format!("setting {perm} on {}", self.child(name).display()))?;
            let mut file = std::fs::File::from(fd);
            file.write_all(contents)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(e) = result {
            self.remove(&tmp);
            return Err(e).with_context(|| format!("writing {}", self.child(name).display()));
        }
        Ok(tmp)
    }

    fn sync(&self) {
        // SAFETY: a valid fd; a failed directory fsync only weakens durability.
        unsafe { libc::fsync(self.fd.as_raw_fd()) };
    }
}

/// `fchown` then `fchmod` (chown may clear set-id bits, so the mode goes last).
fn set_perm(fd: &OwnedFd, perm: Perm) -> std::io::Result<()> {
    // SAFETY: a valid fd.
    if unsafe { libc::fchown(fd.as_raw_fd(), perm.uid, perm.gid) } != 0 {
        return Err(last_os_error());
    }
    // SAFETY: a valid fd.
    if unsafe { libc::fchmod(fd.as_raw_fd(), perm.mode as libc::mode_t) } != 0 {
        return Err(last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me(mode: u32) -> Perm {
        let (uid, gid) = writer();
        Perm { uid, gid, mode }
    }

    #[test]
    fn create_new_never_replaces_and_ignores_the_umask() {
        let t = tempfile::tempdir().unwrap();
        let d = Dir::open_root(t.path()).unwrap();
        assert!(d.create_new("a", b"one", me(0o640)).unwrap());
        assert!(!d.create_new("a", b"two", me(0o600)).unwrap());
        assert_eq!(d.read("a").unwrap(), b"one");
        assert_eq!(d.stat("a").unwrap().unwrap().perm, me(0o640));
        let left: Vec<_> = std::fs::read_dir(t.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary is left behind");
    }

    #[test]
    fn symlinks_are_refused_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), t.path().join("sub")).unwrap();
        std::os::unix::fs::symlink(target.path().join("f"), t.path().join("f")).unwrap();
        let d = Dir::open_root(t.path()).unwrap();
        assert!(d.open_dir("sub").is_err());
        assert!(d.read("f").is_err());
        assert!(!d.create_new("f", b"x", me(0o600)).unwrap());
        assert!(!target.path().join("f").exists());
        assert!(Dir::open_root(&t.path().join("sub")).is_err());
    }
}
