//! #308: a caller's identity token is read only from a regular file its owner alone can read.
//! A symlink, a directory, a FIFO or a group- or world-readable file is refused
//! ([`IdentityError::TokenFileRefused`], which a service treats as fatal at start) with an error
//! naming the file and what's wrong; a missing or empty file stays a [`IdentityError::Config`].

use chelabase_identity::{Identity, IdentityError};
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

/// The refusal for `path`, within 5 s (a FIFO must never block).
fn refused(path: &Path) -> String {
    let p = path.to_path_buf();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(Identity::from_token_file("http://identity:8080", &p).map(drop));
    });
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Err(e @ IdentityError::TokenFileRefused(_))) => {
            let msg = e.to_string();
            assert!(msg.contains(&path.display().to_string()), "{msg}");
            msg
        }
        Ok(other) => panic!("{} should be refused, got {other:?}", path.display()),
        Err(_) => panic!("reading {} blocked", path.display()),
    }
}

// AC 1.
#[test]
fn token_file_reads_a_private_regular_file() {
    let t = dir();
    let path = t.path().join("worker-identity.token");
    write(&path, "the-token\n", 0o600);
    assert!(Identity::from_token_file("http://identity:8080", &path).is_ok());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(Identity::from_token_file("http://identity:8080", &path).is_ok());
}

// AC 2.
#[test]
fn token_file_refuses_a_symlink() {
    let t = dir();
    let real = t.path().join("real.token");
    write(&real, "the-token", 0o600);
    let link = t.path().join("worker-identity.token");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(refused(&link).contains("symlink"));
}

// AC 3.
#[test]
fn token_file_refuses_a_loose_mode() {
    let t = dir();
    let path = t.path().join("worker-identity.token");
    for mode in [0o640, 0o644, 0o604, 0o660] {
        write(&path, "the-token", mode);
        let msg = refused(&path);
        assert!(msg.contains(&format!("{mode:04o}")), "{mode:o}: {msg}");
        assert!(!msg.contains("the-token"), "{msg}");
    }
}

// AC 3.
#[test]
fn token_file_refuses_a_directory_or_a_fifo() {
    let t = dir();
    let d = t.path().join("a-dir");
    std::fs::create_dir(&d).unwrap();
    assert!(refused(&d).contains("not a regular file"));
    let fifo = t.path().join("a-fifo");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(refused(&fifo).contains("not a regular file"));
}

// Missing or empty: not configured yet, a Config error naming the file (callers that treat the
// identity service as optional keep waiting on it).
#[test]
fn a_missing_or_empty_token_file_is_a_config_error() {
    let t = dir();
    let none = t.path().join("none.token");
    match Identity::from_token_file("http://x", &none) {
        Err(IdentityError::Config(msg)) => assert!(msg.contains(&none.display().to_string())),
        other => panic!("{other:?}"),
    }
    let empty = t.path().join("empty.token");
    write(&empty, " \n", 0o600);
    match Identity::from_token_file("http://x", &empty) {
        Err(IdentityError::Config(msg)) => assert!(msg.contains(&empty.display().to_string())),
        other => panic!("{other:?}"),
    }
}
