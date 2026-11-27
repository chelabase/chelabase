//! #308: every key file a constructor loads is read through the strict file checks. A private
//! key (signing key) must be a regular file only its owner can read (0600 or 0400); a public key
//! (verify key) a regular file only its owner can write (0644). Neither may be a symlink, a
//! directory or a FIFO, and the error names the file and what's wrong.

use chelabase_auth::AuthError;
use chelabase_auth::app_token::{AppTokenSigner, AppTokenVerifier};
use chelabase_auth::internal::{InternalSigner, InternalVerifier, generate_keypair_pem};
use chelabase_auth::member_token::{MemberTokenSigner, MemberTokenVerifier};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp() -> Tmp {
    let dir = std::env::temp_dir().join(format!("chelabase-auth-keys-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    Tmp(dir)
}

fn write(path: &Path, contents: &str, mode: u32) {
    let _ = std::fs::remove_file(path);
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// Which file of a constructor's pair is under test.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Half {
    Private,
    Public,
}

/// Each constructor that reads files: a name, the half it reads at `file` (the other half, if
/// any, is a good file), and the load. `Ok(())` when it loaded.
type Load = fn(private: &Path, public: &Path) -> Result<(), AuthError>;

fn loaders() -> Vec<(&'static str, Half, Load)> {
    vec![
        ("InternalSigner::from_pem_file", Half::Private, |k, _| {
            InternalSigner::from_pem_file(k).map(drop)
        }),
        ("InternalVerifier::from_pem_file", Half::Public, |_, p| {
            InternalVerifier::from_pem_file(p).map(drop)
        }),
        (
            "AppTokenSigner::from_pem_files (private)",
            Half::Private,
            |k, p| AppTokenSigner::from_pem_files(k, p).map(drop),
        ),
        (
            "AppTokenSigner::from_pem_files (public)",
            Half::Public,
            |k, p| AppTokenSigner::from_pem_files(k, p).map(drop),
        ),
        ("AppTokenVerifier::from_pem_file", Half::Public, |_, p| {
            AppTokenVerifier::from_pem_file(p).map(drop)
        }),
        (
            "MemberTokenSigner::from_pem_files (private)",
            Half::Private,
            |k, p| MemberTokenSigner::from_pem_files(k, p, "http://auth.test").map(drop),
        ),
        (
            "MemberTokenSigner::from_pem_files (public)",
            Half::Public,
            |k, p| MemberTokenSigner::from_pem_files(k, p, "http://auth.test").map(drop),
        ),
        (
            "MemberTokenVerifier::from_pem_file",
            Half::Public,
            |_, p| MemberTokenVerifier::from_pem_file(p, "http://auth.test").map(drop),
        ),
    ]
}

/// A key dir with a good pair: the private key 0600, the public key 0644.
struct Pair {
    _t: Tmp,
    dir: PathBuf,
    private: PathBuf,
    public: PathBuf,
    private_pem: String,
    public_pem: String,
}

fn pair() -> Pair {
    let t = tmp();
    let dir = t.0.clone();
    let (private_pem, public_pem) = generate_keypair_pem();
    let private = dir.join("key.pem");
    let public = dir.join("key.pub");
    write(&private, &private_pem, 0o600);
    write(&public, &public_pem, 0o644);
    Pair {
        _t: t,
        dir,
        private,
        public,
        private_pem,
        public_pem,
    }
}

/// Runs `load` with the half under test at `bad` and expects a refusal naming `bad` and
/// containing `what`; never waits more than 5 s (a FIFO must not block the caller).
fn expect_refused(name: &str, half: Half, load: Load, p: &Pair, bad: &Path, what: &str) {
    let (private, public) = match half {
        Half::Private => (bad.to_path_buf(), p.public.clone()),
        Half::Public => (p.private.clone(), bad.to_path_buf()),
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(load(&private, &public));
    });
    let got = rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| panic!("{name}: reading {} blocked", bad.display()));
    let msg = match got {
        Err(AuthError::Config(msg)) => msg,
        other => panic!("{name}: {} should be refused, got {other:?}", bad.display()),
    };
    assert!(
        msg.contains(&bad.display().to_string()) && msg.contains(what),
        "{name}: {msg}"
    );
    assert!(!msg.contains("PRIVATE KEY-----\n"), "{name}: {msg}");
}

// AC 1: a 0600 private key and a 0644 public key load as before (0400 too).
#[test]
fn every_loader_reads_a_private_regular_file() {
    let p = pair();
    for (name, _, load) in loaders() {
        load(&p.private, &p.public).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    std::fs::set_permissions(&p.private, std::fs::Permissions::from_mode(0o400)).unwrap();
    std::fs::set_permissions(&p.public, std::fs::Permissions::from_mode(0o444)).unwrap();
    for (name, _, load) in loaders() {
        load(&p.private, &p.public).unwrap_or_else(|e| panic!("{name} (read-only): {e}"));
    }
}

// AC 2: a symlink to a good key is refused, never followed.
#[test]
fn every_loader_refuses_a_symlink() {
    let p = pair();
    for (name, half, load) in loaders() {
        let target = match half {
            Half::Private => &p.private,
            Half::Public => &p.public,
        };
        let link = p.dir.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(target, &link).unwrap();
        expect_refused(name, half, load, &p, &link, "symlink");
    }
}

// AC 3: a directory or a FIFO is refused (and a FIFO never blocks).
#[test]
fn every_loader_refuses_a_directory_or_a_fifo() {
    let p = pair();
    let dir = p.dir.join("a-dir");
    std::fs::create_dir(&dir).unwrap();
    let fifo = p.dir.join("a-fifo");
    mkfifo(&fifo);
    for (name, half, load) in loaders() {
        expect_refused(name, half, load, &p, &dir, "not a regular file");
        expect_refused(name, half, load, &p, &fifo, "not a regular file");
    }
}

// AC 3: a private key its group or others can read is refused with its mode; so is a public key
// its group or others can write. A public key others can read is fine.
#[test]
fn every_loader_refuses_a_loose_mode() {
    let p = pair();
    let loose = p.dir.join("loose");
    for (name, half, load) in loaders() {
        let (pem, modes): (&str, &[u32]) = match half {
            Half::Private => (&p.private_pem, &[0o640, 0o644, 0o604, 0o660]),
            Half::Public => (&p.public_pem, &[0o664, 0o646, 0o666]),
        };
        for &mode in modes {
            write(&loose, pem, mode);
            expect_refused(name, half, load, &p, &loose, &format!("{mode:04o}"));
        }
    }
}

// A group-readable private key is refused even though the old reader took it (the case an
// operator hits first: `chmod 640`).
#[test]
fn read_key_refuses_a_group_readable_file() {
    let p = pair();
    std::fs::set_permissions(&p.private, std::fs::Permissions::from_mode(0o640)).unwrap();
    let err = InternalSigner::from_pem_file(&p.private)
        .err()
        .expect("refused");
    let msg = err.to_string();
    assert!(
        msg.contains(&p.private.display().to_string())
            && msg.contains("0640")
            && msg.contains("chmod 600"),
        "{msg}"
    );
}

#[test]
fn read_key_refuses_a_symlink() {
    let p = pair();
    let link = p.dir.join("signing.pem");
    std::os::unix::fs::symlink(&p.private, &link).unwrap();
    let msg = InternalSigner::from_pem_file(&link)
        .err()
        .expect("refused")
        .to_string();
    assert!(
        msg.contains(&link.display().to_string()) && msg.contains("symlink"),
        "{msg}"
    );
}

// A missing or empty key file is still an error naming the file.
#[test]
fn a_missing_or_empty_key_file_names_the_file() {
    let p = pair();
    let none = p.dir.join("none.pem");
    let msg = InternalSigner::from_pem_file(&none)
        .err()
        .expect("missing")
        .to_string();
    assert!(msg.contains(&none.display().to_string()), "{msg}");
    let empty = p.dir.join("empty.pem");
    write(&empty, "", 0o600);
    let msg = InternalSigner::from_pem_file(&empty)
        .err()
        .expect("empty")
        .to_string();
    assert!(msg.contains(&empty.display().to_string()), "{msg}");
}
