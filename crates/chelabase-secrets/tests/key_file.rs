//! #308: the secrets key (and auth's cookie key, the same format) is read only from a regular
//! file its owner alone can read. A symlink, a directory, a FIFO or a group- or world-readable
//! file is refused with an error naming the file and what's wrong.

use chelabase_secrets::{SecretKey, SecretsError};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

fn dir() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    t
}

fn write(path: &Path, contents: &str, mode: u32) {
    let _ = std::fs::remove_file(path);
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// The error for `path`, which must be refused within 5 s (a FIFO must never block).
fn refused(path: &Path) -> String {
    let p = path.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(SecretKey::from_file(&p).map(drop));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(SecretsError::Config(msg))) => {
            assert!(msg.contains(&path.display().to_string()), "{msg}");
            msg
        }
        Ok(other) => panic!("{} should be refused, got {other:?}", path.display()),
        Err(_) => panic!("reading {} blocked", path.display()),
    }
}

// AC 1.
#[test]
fn from_file_reads_a_private_regular_file() {
    let t = dir();
    let key = SecretKey::generate();
    let path = t.path().join("secrets.key");
    write(&path, &format!("{}\n", key.to_base64()), 0o600);
    assert_eq!(
        SecretKey::from_file(&path).unwrap().to_base64(),
        key.to_base64()
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert_eq!(
        SecretKey::from_file(&path).unwrap().to_base64(),
        key.to_base64()
    );
}

// AC 2.
#[test]
fn from_file_refuses_a_symlink() {
    let t = dir();
    let real = t.path().join("real.key");
    write(&real, &SecretKey::generate().to_base64(), 0o600);
    let link = t.path().join("secrets.key");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let msg = refused(&link);
    assert!(msg.contains("symlink"), "{msg}");
}

// AC 3.
#[test]
fn from_file_refuses_a_loose_mode() {
    let t = dir();
    let path = t.path().join("secrets.key");
    let b64 = SecretKey::generate().to_base64();
    for mode in [0o640, 0o644, 0o604, 0o660, 0o666] {
        write(&path, &b64, mode);
        let msg = refused(&path);
        assert!(
            msg.contains(&format!("{mode:04o}")) && msg.contains("chmod 600"),
            "{mode:o}: {msg}"
        );
        assert!(!msg.contains(&b64), "{msg}");
    }
}

// AC 3.
#[test]
fn from_file_refuses_a_directory_or_a_fifo() {
    let t = dir();
    let d = t.path().join("secrets.key");
    std::fs::create_dir(&d).unwrap();
    assert!(refused(&d).contains("not a regular file"));
    let fifo = t.path().join("fifo.key");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(refused(&fifo).contains("not a regular file"));
}

#[test]
fn from_file_names_a_missing_or_empty_file() {
    let t = dir();
    refused(&t.path().join("none.key"));
    let empty = t.path().join("empty.key");
    write(&empty, "\n", 0o600);
    refused(&empty);
}
