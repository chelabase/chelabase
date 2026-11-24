//! A value an app reads from a file that may appear after it starts: the bootstrap's outputs
//! (`chelabase-init bootstrap --out <dir>`), which a self-hosted stack writes into a volume while
//! the apps are already running.
//!
//! [`FileValue::get`] reads the file on first use and caches its text once the file is present
//! and non-empty; until then it answers [`Unavailable::NotYet`], which a handler turns into
//! 503 `unavailable` (never a pass). A file that is there but unsafe to trust is
//! [`Unavailable::Refused`], also 503, and is read again on the next call (nothing is cached):
//!
//! - a symlink, or anything but a regular file, is refused (never followed);
//! - a file writable by its group or others is refused;
//! - a [`Sensitivity::Secret`] file readable by its group or others is refused (`chmod 600`);
//!   a [`Sensitivity::SharedSecret`] one readable by others (`chmod 640`);
//! - so is a file over [`MAX_LEN`] bytes or one that isn't UTF-8.
//!
//! **Writers must replace a file atomically** (write a temporary file in the same directory, then
//! rename it into place), never write it in place: the first non-empty read is cached until the
//! process restarts, so a reader that catches a half-written file keeps the truncated value.
//! `chelabase-init`'s writers (the secrets and the bootstrap's outputs) already do; an operator
//! writing one by hand should too (`printf … > f.tmp && mv f.tmp f`). The file is opened with
//! `O_NOFOLLOW | O_NONBLOCK`, so a name swapped for a symlink or a FIFO is refused or read again,
//! and never blocks the caller.
//!
//! [`read_private_file`] applies the same rules to a key or token file read once at start (the
//! secrets key, the signing and verify keys, the identity tokens): there a missing, empty or
//! replaced-while-read file is an error too, so a service refuses to start, naming the file.
//!
//! These are the bootstrap's own rules for its output dir (`chelabase-init`'s `fsx`): ids 0644,
//! secrets 0600, no symlinks. Nothing here ever puts the value in a message: errors and `Debug`
//! name the variable and the path only.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// The largest file read; anything bigger is refused.
pub const MAX_LEN: u64 = 64 * 1024;

/// Whether the file holds a secret (only its owner may read it) or a public id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensitivity {
    /// An id: readable by anyone (0644), writable only by its owner.
    Public,
    /// A secret: readable and writable only by its owner (0600).
    Secret,
    /// A secret its owner shares with one consumer group (m1's layout gives
    /// `db/metadata.password` 0640, group postgres): readable by owner and group, never by
    /// others, and writable by its owner only.
    SharedSecret,
}

/// Why [`FileValue::get`] has no value. Both mean 503 `unavailable` to a caller. The message
/// names the variable and the path, never the contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unavailable {
    /// The file doesn't exist yet, or is empty: the bootstrap hasn't written it.
    NotYet { var: String, path: Option<PathBuf> },
    /// The file is there but isn't trusted (see the module docs); read again next time.
    Refused {
        var: String,
        path: PathBuf,
        reason: String,
    },
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unavailable::NotYet {
                var,
                path: Some(path),
            } => {
                write!(f, "{var}={} is not written yet", path.display())
            }
            Unavailable::NotYet { var, path: None } => write!(f, "{var} is empty"),
            Unavailable::Refused { var, path, reason } => {
                write!(f, "refusing {var}={}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for Unavailable {}

enum Source {
    /// From the variable itself (tests and the workbench's env files).
    Fixed,
    File {
        path: PathBuf,
        sensitivity: Sensitivity,
    },
}

struct Inner {
    /// The variable the value came from (`NAME` or `NAME_FILE`), for messages.
    var: String,
    source: Source,
    value: OnceLock<String>,
}

/// A value from `NAME` or from the file `NAME_FILE` names, read when first asked for. Cloning
/// shares the cache.
#[derive(Clone)]
pub struct FileValue(Arc<Inner>);

impl FileValue {
    /// A value known now (the plain `NAME` form). Surrounding whitespace is dropped; a blank
    /// one is never ready.
    pub fn fixed(var: impl Into<String>, value: &str) -> Self {
        let cell = OnceLock::new();
        let value = value.trim();
        if !value.is_empty() {
            let _ = cell.set(value.to_string());
        }
        Self(Arc::new(Inner {
            var: var.into(),
            source: Source::Fixed,
            value: cell,
        }))
    }

    /// A value read from `path` when first asked for.
    pub fn file(
        var: impl Into<String>,
        path: impl Into<PathBuf>,
        sensitivity: Sensitivity,
    ) -> Self {
        Self(Arc::new(Inner {
            var: var.into(),
            source: Source::File {
                path: path.into(),
                sensitivity,
            },
            value: OnceLock::new(),
        }))
    }

    /// `NAME` (fixed) or `NAME_FILE` (read later); `None` when neither is set. Blank values
    /// count as unset. Both set is an error naming `NAME`.
    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        name: &str,
        sensitivity: Sensitivity,
    ) -> Result<Option<Self>, String> {
        let file_var = format!("{name}_FILE");
        let get = |n: &str| get(n).filter(|v| !v.trim().is_empty());
        match (get(name), get(&file_var)) {
            (Some(_), Some(_)) => Err(format!("set {name} or {file_var}, not both")),
            (Some(value), None) => Ok(Some(Self::fixed(name, &value))),
            (None, Some(path)) => Ok(Some(Self::file(file_var, path.trim(), sensitivity))),
            (None, None) => Ok(None),
        }
    }

    /// [`Self::from_lookup`] on the process environment.
    pub fn from_env(name: &str, sensitivity: Sensitivity) -> Result<Option<Self>, String> {
        Self::from_lookup(|n| std::env::var(n).ok(), name, sensitivity)
    }

    /// The variable the value comes from (`NAME` or `NAME_FILE`).
    pub fn var(&self) -> &str {
        &self.0.var
    }

    /// The file's path, for the `_FILE` form.
    pub fn path(&self) -> Option<&Path> {
        match &self.0.source {
            Source::Fixed => None,
            Source::File { path, .. } => Some(path),
        }
    }

    /// The value, reading the file if it isn't cached yet.
    pub fn get(&self) -> Result<&str, Unavailable> {
        if let Some(value) = self.0.value.get() {
            return Ok(value);
        }
        let Source::File { path, sensitivity } = &self.0.source else {
            return Err(Unavailable::NotYet {
                var: self.0.var.clone(),
                path: None,
            });
        };
        match read(path, *sensitivity) {
            Ok(Some(value)) => {
                // A concurrent reader may have set it first; both read the same file.
                let _ = self.0.value.set(value);
                Ok(self.0.value.get().expect("set above"))
            }
            Ok(None) => Err(Unavailable::NotYet {
                var: self.0.var.clone(),
                path: Some(path.clone()),
            }),
            Err(reason) => Err(Unavailable::Refused {
                var: self.0.var.clone(),
                path: path.clone(),
                reason,
            }),
        }
    }

    /// Whether [`Self::get`] has a value now.
    pub fn is_ready(&self) -> bool {
        self.get().is_ok()
    }
}

/// A fixed value (tests). Use [`FileValue::from_lookup`] for configuration.
impl From<&str> for FileValue {
    fn from(value: &str) -> Self {
        Self::fixed("<fixed>", value)
    }
}

/// Never shows the value.
impl fmt::Debug for FileValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileValue")
            .field("var", &self.0.var)
            .field("path", &self.path())
            .field("cached", &self.0.value.get().is_some())
            .finish()
    }
}

/// `Ok(None)`: missing or empty (not yet), or replaced while it was read (read again next
/// time). `Err`: present but refused, with the reason (never the contents).
fn read(path: &Path, sensitivity: Sensitivity) -> Result<Option<String>, String> {
    read_with(path, sensitivity, || {})
}

/// [`read`], with `between` run after the name is checked and before it is opened (tests swap
/// the file there).
fn read_with(
    path: &Path,
    sensitivity: Sensitivity,
    between: impl FnOnce(),
) -> Result<Option<String>, String> {
    match checked_read(path, sensitivity, between) {
        Checked::Text(text) => {
            let value = text.trim();
            Ok((!value.is_empty()).then(|| value.to_string()))
        }
        Checked::Missing | Checked::Replaced => Ok(None),
        Checked::Refused(reason) => Err(reason),
    }
}

/// Why [`read_private_file`] has no contents for a path. Its `Display` names the path and says
/// what's wrong, never the contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileError {
    /// Nothing is at the path.
    Missing { path: PathBuf },
    /// The file is empty, or only whitespace.
    Empty { path: PathBuf },
    /// The file is there but is not trusted (see the module docs), or can't be read.
    Refused { path: PathBuf, reason: String },
}

impl FileError {
    /// The path the error is about.
    pub fn path(&self) -> &Path {
        match self {
            FileError::Missing { path }
            | FileError::Empty { path }
            | FileError::Refused { path, .. } => path,
        }
    }
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileError::Missing { path } => write!(f, "{} does not exist", path.display()),
            FileError::Empty { path } => write!(f, "{} is empty", path.display()),
            FileError::Refused { path, reason } => {
                write!(f, "refusing {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for FileError {}

/// A key or token file read once, at start, under the same rules as [`FileValue`]: never
/// through a symlink, only a regular file, at most [`MAX_LEN`] bytes of UTF-8, never writable
/// by its group or others, and (for [`Sensitivity::Secret`]) never readable by them either
/// (`chmod 600`; 0400 is fine). The checks are made on the opened descriptor, so a name swapped
/// between the check and the open is refused, never read. The text is returned as it is (not
/// trimmed); a blank file is [`FileError::Empty`].
pub fn read_private_file(path: &Path, sensitivity: Sensitivity) -> Result<String, FileError> {
    read_private_file_with(path, sensitivity, || {})
}

/// [`read_private_file`], with `between` run after the name is checked and before it is
/// opened (tests swap the file there).
fn read_private_file_with(
    path: &Path,
    sensitivity: Sensitivity,
    between: impl FnOnce(),
) -> Result<String, FileError> {
    let path_buf = || path.to_path_buf();
    match checked_read(path, sensitivity, between) {
        Checked::Text(text) if text.trim().is_empty() => Err(FileError::Empty { path: path_buf() }),
        Checked::Text(text) => Ok(text),
        Checked::Missing => Err(FileError::Missing { path: path_buf() }),
        Checked::Replaced => Err(FileError::Refused {
            path: path_buf(),
            reason: "it was replaced while it was being read; start again".into(),
        }),
        Checked::Refused(reason) => Err(FileError::Refused {
            path: path_buf(),
            reason,
        }),
    }
}

/// What [`checked_read`] found.
enum Checked {
    /// The whole file, as text.
    Text(String),
    /// Nothing at the name (before or at the open).
    Missing,
    /// What was opened is not what was checked: the name was replaced in between.
    Replaced,
    /// Present but refused, with the reason (never the contents).
    Refused(String),
}

/// The one implementation of the module's rules, shared by [`FileValue`] and
/// [`read_private_file`].
fn checked_read(path: &Path, sensitivity: Sensitivity, between: impl FnOnce()) -> Checked {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let not_found = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    // The name itself, not what it points at.
    let named = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if not_found(&e) => return Checked::Missing,
        Err(e) => return Checked::Refused(format!("reading it: {e}")),
    };
    if named.file_type().is_symlink() {
        return Checked::Refused("it is a symlink".into());
    }
    if !named.is_file() {
        return Checked::Refused("it is not a regular file".into());
    }
    between();
    // Never follows a symlink and never waits (a FIFO swapped in would otherwise block until a
    // writer came); what was opened is checked below.
    let opened_file = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
    };
    let mut file = match opened_file {
        Ok(f) => f,
        Err(e) if not_found(&e) => return Checked::Missing,
        // ELOOP: the name became a symlink since the check.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Checked::Refused("it is a symlink".into());
        }
        Err(e) => return Checked::Refused(format!("opening it: {e}")),
    };
    // What is checked below is the file that was opened: if the name now points elsewhere
    // (replaced, or swapped for a symlink, since the check above), it isn't read.
    let opened = match file.metadata() {
        Ok(m) => m,
        Err(e) => return Checked::Refused(format!("reading it: {e}")),
    };
    if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) || !opened.is_file() {
        return Checked::Replaced;
    }
    let mode = opened.mode() & 0o7777;
    if mode & 0o022 != 0 {
        // A secret's fix is the same whichever bits are loose.
        let fix = match sensitivity {
            Sensitivity::Secret => "chmod 600",
            Sensitivity::SharedSecret => "chmod 640 (or 600)",
            Sensitivity::Public => "chmod go-w",
        };
        return Checked::Refused(format!(
            "it is writable by its group or others (mode {mode:04o}); {fix} it"
        ));
    }
    if sensitivity == Sensitivity::SharedSecret && mode & 0o007 != 0 {
        return Checked::Refused(format!(
            "it holds a secret but is readable by others (mode {mode:04o}); chmod 640 (or 600) it"
        ));
    }
    if sensitivity == Sensitivity::Secret && mode & 0o077 != 0 {
        return Checked::Refused(format!(
            "it holds a secret but is readable by its group or others (mode {mode:04o}); chmod 600 it"
        ));
    }
    if opened.len() > MAX_LEN {
        return Checked::Refused(format!("it is larger than {MAX_LEN} bytes"));
    }
    let mut bytes = Vec::new();
    if let Err(e) = file.by_ref().take(MAX_LEN + 1).read_to_end(&mut bytes) {
        return Checked::Refused(format!("reading it: {e}"));
    }
    if bytes.len() as u64 > MAX_LEN {
        return Checked::Refused(format!("it is larger than {MAX_LEN} bytes"));
    }
    match String::from_utf8(bytes) {
        Ok(text) => Checked::Text(text),
        Err(_) => Checked::Refused("it is not UTF-8 text".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Tmp(PathBuf);

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn tmp() -> Tmp {
        let dir =
            std::env::temp_dir().join(format!("chelabase-file-value-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        Tmp(dir)
    }

    fn write(path: &Path, contents: &str, mode: u32) {
        // A read-only file from the last round can't be written over.
        let _ = std::fs::remove_file(path);
        std::fs::write(path, contents).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn refused(v: &FileValue) -> String {
        match v.get() {
            Err(e @ Unavailable::Refused { .. }) => e.to_string(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    // AC 1: absent, then empty, then present; once read, the value stays (the file changing
    // or going away doesn't change it).
    #[test]
    fn not_yet_then_cached() {
        let t = tmp();
        let path = t.0.join("some-id");
        let v = FileValue::file("CHELABASE_SOME_ID_FILE", &path, Sensitivity::Public);
        assert!(matches!(v.get(), Err(Unavailable::NotYet { .. })), "absent");
        assert!(!v.is_ready());
        write(&path, "", 0o644);
        assert!(matches!(v.get(), Err(Unavailable::NotYet { .. })), "empty");
        write(&path, " \n\n", 0o644);
        assert!(matches!(v.get(), Err(Unavailable::NotYet { .. })), "blank");
        write(&path, "org-1\n", 0o644);
        assert_eq!(v.get(), Ok("org-1"));
        assert!(v.is_ready());
        write(&path, "org-2\n", 0o644);
        assert_eq!(v.get(), Ok("org-1"), "cached once present");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(v.get(), Ok("org-1"));
        // A clone shares the cache.
        assert_eq!(v.clone().get(), Ok("org-1"));
    }

    #[test]
    fn a_secret_file_is_read_at_0600() {
        let t = tmp();
        let path = t.0.join("actions-signing-key");
        write(&path, "the-key\n", 0o600);
        let v = FileValue::file("K_FILE", &path, Sensitivity::Secret);
        assert_eq!(v.get(), Ok("the-key"));
        let path = t.0.join("read-only");
        write(&path, "ro-key", 0o400);
        assert_eq!(
            FileValue::file("K_FILE", &path, Sensitivity::Secret).get(),
            Ok("ro-key")
        );
    }

    // m1's rule: a symlink is refused, never followed, even to a good file.
    #[test]
    fn a_symlink_is_refused() {
        let t = tmp();
        let target = t.0.join("real");
        write(&target, "secret-contents", 0o600);
        let link = t.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        for s in [Sensitivity::Public, Sensitivity::Secret] {
            let v = FileValue::file("K_FILE", &link, s);
            let msg = refused(&v);
            assert!(msg.contains("symlink") && msg.contains("K_FILE"), "{msg}");
            assert!(!msg.contains("secret-contents"), "{msg}");
        }
        // A dangling symlink is refused too, not "not yet".
        let dangling = t.0.join("dangling");
        std::os::unix::fs::symlink(t.0.join("nowhere"), &dangling).unwrap();
        let msg = refused(&FileValue::file("K_FILE", &dangling, Sensitivity::Public));
        assert!(msg.contains("symlink"), "{msg}");
    }

    // N1: a FIFO swapped in between the check and the open must not block the caller (a
    // tokio worker): the open doesn't wait for a writer, and the opened file is refused (or
    // read again later) because it isn't the regular file that was checked.
    #[test]
    fn a_fifo_swapped_in_never_blocks() {
        let t = tmp();
        let path = t.0.join("org-id");
        write(&path, "o1", 0o644);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let got = read_with(&p, Sensitivity::Public, || {
                std::fs::remove_file(&p).unwrap();
                let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
                // SAFETY: a valid NUL-terminated path.
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
            });
            let _ = done_tx.send(got);
        });
        let got = done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("reading a FIFO swapped in blocked the caller");
        assert!(!matches!(got, Ok(Some(_))), "{got:?}");
    }

    #[test]
    fn a_directory_is_refused() {
        let t = tmp();
        let dir = t.0.join("org-id");
        std::fs::create_dir(&dir).unwrap();
        let msg = refused(&FileValue::file("K_FILE", &dir, Sensitivity::Public));
        assert!(msg.contains("not a regular file"), "{msg}");
    }

    // Ids may be world-readable but never writable by others; secrets are the owner's only.
    #[test]
    fn loose_modes_are_refused() {
        let t = tmp();
        let path = t.0.join("f");
        for (mode, s, ok) in [
            (0o644, Sensitivity::Public, true),
            (0o444, Sensitivity::Public, true),
            (0o600, Sensitivity::Public, true),
            (0o664, Sensitivity::Public, false),
            (0o646, Sensitivity::Public, false),
            (0o600, Sensitivity::SharedSecret, true),
            (0o640, Sensitivity::SharedSecret, true),
            (0o644, Sensitivity::SharedSecret, false),
            (0o604, Sensitivity::SharedSecret, false),
            (0o660, Sensitivity::SharedSecret, false),
            (0o600, Sensitivity::Secret, true),
            (0o400, Sensitivity::Secret, true),
            (0o640, Sensitivity::Secret, false),
            (0o604, Sensitivity::Secret, false),
            (0o644, Sensitivity::Secret, false),
        ] {
            write(&path, "value-xyz", mode);
            let v = FileValue::file("K_FILE", &path, s);
            if ok {
                assert_eq!(v.get(), Ok("value-xyz"), "{mode:o} {s:?}");
            } else {
                let msg = refused(&v);
                assert!(
                    msg.contains(&format!("{mode:04o}")),
                    "{mode:o} {s:?}: {msg}"
                );
                assert!(!msg.contains("value-xyz"), "{msg}");
            }
        }
    }

    // A refusal isn't cached: fixing the file makes the next call succeed.
    #[test]
    fn a_refusal_is_read_again() {
        let t = tmp();
        let path = t.0.join("k");
        write(&path, "k1", 0o644);
        let v = FileValue::file("K_FILE", &path, Sensitivity::Secret);
        refused(&v);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(v.get(), Ok("k1"));
    }

    #[test]
    fn oversized_or_non_utf8_files_are_refused() {
        let t = tmp();
        let big = t.0.join("big");
        write(&big, &"x".repeat(MAX_LEN as usize + 1), 0o644);
        let msg = refused(&FileValue::file("K_FILE", &big, Sensitivity::Public));
        assert!(msg.contains("larger"), "{msg}");
        let bin = t.0.join("bin");
        std::fs::write(&bin, [0xff, 0xfe, 0x00]).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o644)).unwrap();
        let msg = refused(&FileValue::file("K_FILE", &bin, Sensitivity::Public));
        assert!(msg.contains("UTF-8"), "{msg}");
    }

    // AC 3: the plain form stays for tests, but both forms at once are refused, naming NAME.
    #[test]
    fn config_refuses_env_and_file_both() {
        let err = FileValue::from_lookup(
            vars(&[
                ("CHELABASE_SOME_ID", "o1"),
                ("CHELABASE_SOME_ID_FILE", "/x/some-id"),
            ]),
            "CHELABASE_SOME_ID",
            Sensitivity::Public,
        )
        .unwrap_err();
        assert!(
            err.contains("CHELABASE_SOME_ID ") && err.contains("not both"),
            "{err}"
        );
        assert!(!err.contains("o1"), "{err}");
        assert!(
            FileValue::from_lookup(vars(&[]), "X", Sensitivity::Public)
                .unwrap()
                .is_none()
        );
        // Blank counts as unset, either form.
        let fixed = FileValue::from_lookup(
            vars(&[("X", " v "), ("X_FILE", " ")]),
            "X",
            Sensitivity::Public,
        )
        .unwrap()
        .unwrap();
        assert_eq!((fixed.var(), fixed.get()), ("X", Ok("v")));
        assert!(fixed.path().is_none());
        let file = FileValue::from_lookup(
            vars(&[("X", ""), ("X_FILE", " /out/x ")]),
            "X",
            Sensitivity::Public,
        )
        .unwrap()
        .unwrap();
        assert_eq!(file.var(), "X_FILE");
        assert_eq!(file.path(), Some(Path::new("/out/x")));
    }

    fn private(path: &Path, s: Sensitivity) -> Result<String, FileError> {
        read_private_file(path, s)
    }

    // #308: the start-time reader takes what FileValue takes, untrimmed.
    #[test]
    fn read_private_file_reads_a_good_file_untrimmed() {
        let t = tmp();
        let path = t.0.join("k");
        write(&path, "-----BEGIN-----\nabc\n-----END-----\n", 0o600);
        assert_eq!(
            private(&path, Sensitivity::Secret).unwrap(),
            "-----BEGIN-----\nabc\n-----END-----\n"
        );
        write(&path, "pub\n", 0o644);
        assert_eq!(private(&path, Sensitivity::Public).unwrap(), "pub\n");
        write(&path, "ro", 0o400);
        assert_eq!(private(&path, Sensitivity::Secret).unwrap(), "ro");
    }

    // Missing and blank are errors naming the file (FileValue's "not yet" is not a pass at start).
    #[test]
    fn read_private_file_refuses_missing_and_blank() {
        let t = tmp();
        let path = t.0.join("k");
        let err = private(&path, Sensitivity::Secret).unwrap_err();
        assert_eq!(err, FileError::Missing { path: path.clone() });
        assert!(err.to_string().contains(&path.display().to_string()));
        for blank in ["", " \n\t"] {
            write(&path, blank, 0o600);
            let err = private(&path, Sensitivity::Secret).unwrap_err();
            assert_eq!(err, FileError::Empty { path: path.clone() });
            assert!(err.to_string().contains("is empty"), "{err}");
        }
    }

    // Every refusal FileValue makes, read_private_file makes too, naming the path and the
    // problem, never the contents.
    #[test]
    fn read_private_file_refuses_what_file_value_refuses() {
        let t = tmp();
        let target = t.0.join("real");
        write(&target, "secret-contents", 0o600);
        let link = t.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dir = t.0.join("dir");
        std::fs::create_dir(&dir).unwrap();
        let fifo = t.0.join("fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let loose = t.0.join("loose");
        let cases: Vec<(PathBuf, Sensitivity, &str, Option<u32>)> = vec![
            (link.clone(), Sensitivity::Secret, "symlink", None),
            (link.clone(), Sensitivity::Public, "symlink", None),
            (dir.clone(), Sensitivity::Secret, "not a regular file", None),
            (
                fifo.clone(),
                Sensitivity::Secret,
                "not a regular file",
                None,
            ),
            (loose.clone(), Sensitivity::Secret, "chmod 600", Some(0o640)),
            (loose.clone(), Sensitivity::Secret, "chmod 600", Some(0o644)),
            (loose.clone(), Sensitivity::Secret, "chmod 600", Some(0o604)),
            (
                loose.clone(),
                Sensitivity::Public,
                "chmod go-w",
                Some(0o664),
            ),
            (
                loose.clone(),
                Sensitivity::Public,
                "chmod go-w",
                Some(0o646),
            ),
            (
                loose.clone(),
                Sensitivity::SharedSecret,
                "chmod 640",
                Some(0o644),
            ),
        ];
        for (path, s, what, mode) in cases {
            if let Some(mode) = mode {
                write(&path, "secret-contents", mode);
            }
            let err = private(&path, s).unwrap_err();
            let msg = err.to_string();
            assert!(matches!(err, FileError::Refused { .. }), "{msg}");
            assert_eq!(err.path(), path.as_path());
            assert!(
                msg.contains(&path.display().to_string()) && msg.contains(what),
                "{msg}"
            );
            if let Some(mode) = mode {
                assert!(msg.contains(&format!("{mode:04o}")), "{msg}");
            }
            assert!(!msg.contains("secret-contents"), "{msg}");
        }
    }

    // The check is on the opened descriptor: a name replaced between the check and the open
    // (by another regular file, a symlink or a FIFO) is refused, never read, and never blocks.
    #[test]
    fn read_private_file_refuses_a_name_swapped_after_the_check() {
        let t = tmp();
        let path = t.0.join("k");
        let other = t.0.join("other");
        write(&other, "other-contents", 0o600);
        type Swap = fn(&Path, &Path);
        let swaps: [(&str, Swap); 3] = [
            ("another file", |p, o| {
                std::fs::remove_file(p).unwrap();
                std::fs::hard_link(o, p).unwrap();
            }),
            ("a symlink", |p, o| {
                std::fs::remove_file(p).unwrap();
                std::os::unix::fs::symlink(o, p).unwrap();
            }),
            ("a FIFO", |p, _| {
                std::fs::remove_file(p).unwrap();
                let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
                // SAFETY: a valid NUL-terminated path.
                assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            }),
        ];
        for (what, swap) in swaps {
            let _ = std::fs::remove_file(&path);
            write(&path, "checked", 0o600);
            let (tx, rx) = std::sync::mpsc::channel();
            let (p, o) = (path.clone(), other.clone());
            std::thread::spawn(move || {
                let _ = tx.send(read_private_file_with(&p, Sensitivity::Secret, || {
                    swap(&p, &o)
                }));
            });
            let got = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_else(|_| panic!("{what}: the read blocked"));
            match got {
                Err(FileError::Refused { reason, .. }) => {
                    assert!(!reason.contains("contents"), "{what}: {reason}")
                }
                other => panic!("{what}: expected a refusal, got {other:?}"),
            }
        }
    }

    #[test]
    fn neither_debug_nor_errors_show_the_value() {
        let t = tmp();
        let path = t.0.join("k");
        write(&path, "very-secret-value", 0o600);
        let v = FileValue::file("K_FILE", &path, Sensitivity::Secret);
        assert_eq!(v.get(), Ok("very-secret-value"));
        let shown = format!("{v:?}");
        assert!(
            !shown.contains("very-secret-value") && shown.contains("K_FILE"),
            "{shown}"
        );
        let fixed = FileValue::fixed("K", "fixed-secret");
        assert!(!format!("{fixed:?}").contains("fixed-secret"));
        let blank = FileValue::fixed("K", "  ");
        assert!(matches!(blank.get(), Err(Unavailable::NotYet { .. })));
    }
}
